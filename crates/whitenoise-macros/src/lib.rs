//! Proc-macro helpers for `whitenoise` performance instrumentation.
//!
//! # Internal-only crate
//!
//! This crate is intentionally `publish = false`.  Every macro it provides
//! expands to code that references `crate::perf_span!(...)`, which is only in
//! scope inside the `whitenoise` crate itself.  Attempting to use these macros
//! from any other crate will produce a compile error in the generated code.
//! If you ever need to make this crate reusable, replace the hard-coded
//! `crate::` path with a fully-qualified path or a re-export strategy.

use proc_macro::TokenStream;
use quote::quote;
use syn::{
    Ident, ItemFn, LitStr, Token,
    parse::{Parse, ParseStream},
    parse_macro_input,
};

/// Parsed arguments for `#[perf_instrument(...)]`.
///
/// Supported forms:
/// ```text
/// #[perf_instrument("prefix")]
/// #[perf_instrument("prefix", name = "explicit::span::name")]
/// ```
struct PerfInstrumentArgs {
    prefix: LitStr,
    name_override: Option<LitStr>,
}

impl Parse for PerfInstrumentArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let prefix: LitStr = input.parse().map_err(|e| {
            syn::Error::new(
                e.span(),
                "perf_instrument requires a prefix string argument, \
                 e.g. #[perf_instrument(\"my_module\")]",
            )
        })?;

        let name_override = if input.peek(Token![,]) {
            let _comma: Token![,] = input.parse()?;

            // Allow a trailing comma with nothing after it.
            if input.is_empty() {
                None
            } else {
                let key: Ident = input.parse().map_err(|e| {
                    syn::Error::new(e.span(), "perf_instrument: expected `name` after `,`")
                })?;

                if key != "name" {
                    return Err(syn::Error::new(
                        key.span(),
                        format!(
                            "perf_instrument: unknown parameter `{key}`; only `name` is supported"
                        ),
                    ));
                }

                let _eq: Token![=] = input.parse().map_err(|e| {
                    syn::Error::new(e.span(), "perf_instrument: expected `=` after `name`")
                })?;

                let value: LitStr = input.parse().map_err(|e| {
                    syn::Error::new(
                        e.span(),
                        "perf_instrument: `name` value must be a string literal, \
                         e.g. name = \"my_module::MyType::my_fn\"",
                    )
                })?;

                Some(value)
            }
        } else {
            None
        };

        Ok(Self {
            prefix,
            name_override,
        })
    }
}

/// Instruments a function with automatic `perf_span!` timing.
///
/// Takes a prefix string and generates a span named `"prefix::function_name"`.
/// Works with both sync and async functions.
///
/// An optional `name = "..."` parameter overrides the generated span name
/// entirely, which is useful when the default `prefix::fn_name` would collide
/// across different impl blocks.
///
/// # Examples
///
/// ```ignore
/// #[perf_instrument("accounts")]
/// async fn create_identity(&self) -> Result<Account> {
///     // automatically gets: let _perf_span = perf_span!("accounts::create_identity");
///     let keys = Keys::generate();
///     // ...
/// }
///
/// #[perf_instrument("db", name = "db::AccountRow::find_by_pubkey")]
/// fn find_by_pubkey(pubkey: &PublicKey) -> Result<Account> {
///     // uses the explicit name override instead of "db::find_by_pubkey"
///     // ...
/// }
/// ```
#[proc_macro_attribute]
pub fn perf_instrument(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as PerfInstrumentArgs);
    let mut func = parse_macro_input!(item as ItemFn);

    let span_name = match args.name_override {
        Some(lit) => lit.value(),
        None => {
            let fn_name = func.sig.ident.to_string();
            format!("{}::{}", args.prefix.value(), fn_name)
        }
    };

    let body = &func.block;
    let new_body = quote! {
        {
            let _perf_span = crate::perf_span!(#span_name);
            #body
        }
    };

    func.block = match syn::parse2(new_body) {
        Ok(block) => block,
        Err(e) => return e.to_compile_error().into(),
    };

    quote! { #func }.into()
}
