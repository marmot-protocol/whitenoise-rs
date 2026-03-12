use proc_macro::TokenStream;
use quote::quote;
use syn::{ItemFn, LitStr, parse_macro_input};

/// Instruments a function with automatic `perf_span!` timing.
///
/// Takes a prefix string and generates a span named `"prefix::function_name"`.
/// Works with both sync and async functions.
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
/// #[perf_instrument("db")]
/// fn find_by_pubkey(pubkey: &PublicKey) -> Result<Account> {
///     // automatically gets: let _perf_span = perf_span!("db::find_by_pubkey");
///     // ...
/// }
/// ```
#[proc_macro_attribute]
pub fn perf_instrument(attr: TokenStream, item: TokenStream) -> TokenStream {
    let prefix = parse_macro_input!(attr as LitStr);
    let mut func = parse_macro_input!(item as ItemFn);

    let fn_name = func.sig.ident.to_string();
    let span_name = format!("{}::{}", prefix.value(), fn_name);

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
