use proc_macro::TokenStream;
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
    let mut func = parse_macro_input!(item as ItemFn);

    // Parse attribute: either just a prefix string, or prefix + name override
    // Supported forms:
    //   #[perf_instrument("prefix")]
    //   #[perf_instrument("prefix", name = "explicit::span::name")]
    let span_name = {
        let attr2: proc_macro2::TokenStream = attr.into();
        let mut tokens = attr2.into_iter();

        // First token group: the prefix literal
        let prefix_tt = tokens
            .next()
            .expect("perf_instrument: expected a prefix string argument");
        let prefix: LitStr = syn::parse2(prefix_tt.into())
            .expect("perf_instrument: prefix must be a string literal");

        // Check for optional `, name = "..."` override
        let mut explicit_name: Option<String> = None;

        // Skip comma if present
        if let Some(comma) = tokens.next() {
            let comma_str = comma.to_string();
            if comma_str == "," {
                // Expect `name = "..."`
                let ident = tokens
                    .next()
                    .expect("perf_instrument: expected `name` after comma");
                assert!(
                    ident.to_string() == "name",
                    "perf_instrument: expected `name` parameter, got `{}`",
                    ident
                );
                let eq = tokens
                    .next()
                    .expect("perf_instrument: expected `=` after `name`");
                assert!(
                    eq.to_string() == "=",
                    "perf_instrument: expected `=`, got `{}`",
                    eq
                );
                let value_tt = tokens
                    .next()
                    .expect("perf_instrument: expected string value after `name =`");
                let value: LitStr = syn::parse2(value_tt.into())
                    .expect("perf_instrument: `name` value must be a string literal");
                explicit_name = Some(value.value());
            }
        }

        match explicit_name {
            Some(name) => name,
            None => {
                let fn_name = func.sig.ident.to_string();
                format!("{}::{}", prefix.value(), fn_name)
            }
        }
    };

    let body = &func.block;

    let new_body = quote! {
        {
            let _perf_span = crate::perf_span!(#span_name);
            #body
        }
    };

    func.block = syn::parse2(new_body).expect("perf_instrument: failed to parse generated block");

    quote! { #func }.into()
}
