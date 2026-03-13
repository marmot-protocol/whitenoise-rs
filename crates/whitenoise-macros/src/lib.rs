use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::{ItemFn, LitStr, parse_macro_input};

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
    let func = parse_macro_input!(item as ItemFn);

    // Parse attribute: either just a prefix string, or prefix + name override
    // Supported forms:
    //   #[perf_instrument("prefix")]
    //   #[perf_instrument("prefix", name = "explicit::span::name")]
    let span_name = match parse_span_name(attr.into(), &func) {
        Ok(name) => name,
        Err(e) => return e.to_compile_error().into(),
    };

    let mut func = func;
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

fn parse_span_name(attr: proc_macro2::TokenStream, func: &ItemFn) -> Result<String, syn::Error> {
    let mut tokens = attr.into_iter();

    // First token: the prefix literal
    let prefix_tt = tokens.next().ok_or_else(|| {
        syn::Error::new(
            Span::call_site(),
            "perf_instrument requires a prefix string argument, e.g. #[perf_instrument(\"my_module\")]",
        )
    })?;

    let prefix: LitStr = syn::parse2(prefix_tt.into()).map_err(|_| {
        syn::Error::new(
            Span::call_site(),
            "perf_instrument: the first argument must be a string literal, e.g. \"my_module\"",
        )
    })?;

    // Check for optional `, name = "..."` override
    let mut explicit_name: Option<String> = None;

    if let Some(comma) = tokens.next() {
        if comma.to_string() != "," {
            return Err(syn::Error::new(
                comma.span(),
                "perf_instrument: expected `,` after prefix, or end of attribute",
            ));
        }

        // Expect `name`
        let ident = tokens.next().ok_or_else(|| {
            syn::Error::new(
                Span::call_site(),
                "perf_instrument: expected `name` keyword after `,`",
            )
        })?;
        if ident.to_string() != "name" {
            return Err(syn::Error::new(
                ident.span(),
                format!("perf_instrument: unknown parameter `{ident}`; only `name` is supported"),
            ));
        }

        // Expect `=`
        let eq = tokens.next().ok_or_else(|| {
            syn::Error::new(
                Span::call_site(),
                "perf_instrument: expected `=` after `name`",
            )
        })?;
        if eq.to_string() != "=" {
            return Err(syn::Error::new(
                eq.span(),
                format!("perf_instrument: expected `=`, got `{eq}`"),
            ));
        }

        // Expect the string value
        let value_tt = tokens.next().ok_or_else(|| {
            syn::Error::new(
                Span::call_site(),
                "perf_instrument: expected a string literal after `name =`",
            )
        })?;
        let value: LitStr = syn::parse2(value_tt.into()).map_err(|_| {
            syn::Error::new(
                Span::call_site(),
                "perf_instrument: `name` value must be a string literal, e.g. name = \"my_module::MyType::my_fn\"",
            )
        })?;
        explicit_name = Some(value.value());
    }

    Ok(match explicit_name {
        Some(name) => name,
        None => {
            let fn_name = func.sig.ident.to_string();
            format!("{}::{}", prefix.value(), fn_name)
        }
    })
}
