use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use heck::ToSnakeCase;
use proc_macro2::TokenStream;
use prost_build::{Method, Service, ServiceGenerator};
use quote::{format_ident, quote};

fn collect_proto_layout(proto_root: &Path) -> Result<(Vec<PathBuf>, Vec<String>), Box<dyn std::error::Error>> {
    fn visit(
        dir: &Path, proto_files: &mut Vec<PathBuf>, proto_packages: &mut BTreeSet<String>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                visit(&path, proto_files, proto_packages)?;
            } else if path.extension().is_some_and(|ext| ext == "proto") {
                let package = dir
                    .iter()
                    .map(|segment| segment.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(".");
                proto_files.push(path);
                proto_packages.insert(format!(".{package}"));
            }
        }

        Ok(())
    }

    let mut proto_files = Vec::new();
    let mut proto_packages = BTreeSet::new();
    visit(proto_root, &mut proto_files, &mut proto_packages)?;
    proto_files.sort();

    if proto_files.is_empty() {
        return Err(std::io::Error::other(format!("no proto files found under {}", proto_root.display())).into());
    }

    Ok((proto_files, proto_packages.into_iter().collect()))
}

struct JsonGrpcCompositeServiceGenerator {
    inner: Box<dyn ServiceGenerator>,
}

impl JsonGrpcCompositeServiceGenerator {
    fn new(inner: Box<dyn ServiceGenerator>) -> Self {
        Self { inner }
    }
}

impl ServiceGenerator for JsonGrpcCompositeServiceGenerator {
    fn generate(&mut self, service: Service, buf: &mut String) {
        let extension = generate_json_grpc_extension(&service);
        self.inner.generate(service, buf);
        buf.push('\n');
        buf.push_str(&extension.to_string());
        buf.push('\n');
    }

    fn finalize(&mut self, buf: &mut String) {
        self.inner.finalize(buf);
    }

    fn finalize_package(&mut self, package: &str, buf: &mut String) {
        self.inner.finalize_package(package, buf);
    }
}

fn generate_json_grpc_extension(service: &Service) -> TokenStream {
    let trait_ident = format_ident!("{}", service.name);
    let server_module_ident = format_ident!("{}_server", service.name.to_snake_case());
    let server_ident = format_ident!("{}Server", service.name);
    let composite_ident = format_ident!("{}JsonGrpcServer", service.name);

    let method_arms = service.methods.iter().map(|method| {
        let path = canonical_path(service, method);
        let request_type = rust_type_tokens(&method.input_type, &service.package);
        let response_type = rust_type_tokens(&method.output_type, &service.package);
        let method_ident = format_ident!("{}", method.name);

        if method.client_streaming {
            quote! {
                #path => {
                    Box::pin(async {
                        Ok(crate::json_grpc::unsupported_streaming_kind_response())
                    })
                }
            }
        } else if method.server_streaming {
            quote! {
                #path => {
                    let inner = std::sync::Arc::clone(&self.inner);
                    let fut = async move {
                        let request: #request_type = match crate::json_grpc::decode_json_body(req).await {
                            Ok(request) => request,
                            Err(status) => return Ok(crate::json_grpc::json_error_response(status)),
                        };

                        match <T as #server_module_ident::#trait_ident>::#method_ident(&inner, tonic::Request::new(request)).await {
                            Ok(response) => {
                                Ok(crate::json_grpc::json_stream_response::<_, #response_type>(response.into_inner()))
                            }
                            Err(status) => Ok(crate::json_grpc::json_error_response(status)),
                        }
                    };

                    Box::pin(fut)
                }
            }
        } else {
            quote! {
                #path => {
                    let inner = std::sync::Arc::clone(&self.inner);
                    let fut = async move {
                        let request: #request_type = match crate::json_grpc::decode_json_body(req).await {
                            Ok(request) => request,
                            Err(status) => return Ok(crate::json_grpc::json_error_response(status)),
                        };

                        match <T as #server_module_ident::#trait_ident>::#method_ident(&inner, tonic::Request::new(request)).await {
                            Ok(response) => Ok(crate::json_grpc::unary_json_response::<#response_type>(&response.into_inner())),
                            Err(status) => Ok(crate::json_grpc::json_error_response(status)),
                        }
                    };

                    Box::pin(fut)
                }
            }
        }
    });

    quote! {
        /// Composite service that routes JSON RPC requests before falling back to tonic gRPC.
        #[derive(Clone)]
        pub struct #composite_ident<T> {
            inner: std::sync::Arc<T>,
            grpc: #server_module_ident::#server_ident<T>,
        }

        impl<T> #composite_ident<T> {
            /// Creates a composite service from an owned service implementation.
            pub fn new(inner: T) -> Self {
                Self::from_arc(std::sync::Arc::new(inner))
            }

            /// Creates a composite service from a shared service implementation.
            pub fn from_arc(inner: std::sync::Arc<T>) -> Self {
                Self {
                    inner: inner.clone(),
                    grpc: #server_module_ident::#server_ident::from_arc(inner),
                }
            }
        }

        impl<T, B> tonic::codegen::Service<http::Request<B>> for #composite_ident<T>
        where
            T: #server_module_ident::#trait_ident,
            B: tonic::codegen::Body<Data = bytes::Bytes> + std::marker::Send + 'static,
            B::Error: Into<tonic::codegen::StdError> + std::marker::Send + 'static,
        {
            type Response = http::Response<tonic::body::Body>;
            type Error = std::convert::Infallible;
            type Future = tonic::codegen::BoxFuture<Self::Response, Self::Error>;

            fn poll_ready(&mut self, _cx: &mut std::task::Context<'_>) -> std::task::Poll<std::result::Result<(), Self::Error>> {
                std::task::Poll::Ready(Ok(()))
            }

            fn call(&mut self, req: http::Request<B>) -> Self::Future {
                if !crate::json_grpc::is_json_content_type(req.headers()) {
                    return self.grpc.call(req);
                }

                if req.method() != http::Method::POST {
                    return Box::pin(async move {
                        Ok(crate::json_grpc::method_not_allowed_response())
                    });
                }

                match req.uri().path() {
                    #(#method_arms,)*
                    _ => {
                        Box::pin(async move {
                            Ok(crate::json_grpc::unknown_rpc_path_response())
                        })
                    }
                }
            }
        }

        impl<T> tonic::server::NamedService for #composite_ident<T> {
            const NAME: &'static str = #server_module_ident::SERVICE_NAME;
        }
    }
}

fn rust_type_tokens(raw: &str, package: &str) -> TokenStream {
    let ty = raw.trim_start_matches('.');
    let package_prefix = format!("{package}.");
    let stripped = ty.strip_prefix(&package_prefix).unwrap_or(ty);
    TokenStream::from_str(&stripped.replace('.', "::")).expect("valid generated Rust type path")
}

fn canonical_path(service: &Service, method: &Method) -> String {
    format!("/{}/{method}", service_name(service), method = method.proto_name)
}

fn service_name(service: &Service) -> String {
    if service.package.is_empty() {
        service.proto_name.clone()
    } else {
        format!("{}.{}", service.package, service.proto_name)
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_root = PathBuf::from("lyquor");

    let (proto_files, proto_packages) = collect_proto_layout(&proto_root)?;

    println!("cargo:rerun-if-changed={}", proto_root.display());
    for proto_file in &proto_files {
        println!("cargo:rerun-if-changed={}", proto_file.display());
    }

    if std::env::var("PROTOC").is_err() &&
        let Ok(path) = protoc_bin_vendored::protoc_bin_path()
    {
        unsafe {
            std::env::set_var("PROTOC", path);
        }
    }

    let descriptor_path = PathBuf::from(std::env::var("OUT_DIR")?).join("lyquor-descriptor.bin");

    let tonic_builder = tonic_prost_build::configure().build_server(true).build_client(true);
    let mut config = prost_build::Config::new();
    config.file_descriptor_set_path(&descriptor_path);
    config.service_generator(Box::new(JsonGrpcCompositeServiceGenerator::new(
        tonic_builder.service_generator(),
    )));
    config.compile_protos(&proto_files, &[PathBuf::from_str(".").unwrap()])?;

    let descriptor_set = std::fs::read(&descriptor_path)?;
    pbjson_build::Builder::new()
        .register_descriptors(&descriptor_set)?
        .build(&proto_packages)?;

    Ok(())
}
