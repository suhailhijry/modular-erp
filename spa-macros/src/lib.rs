use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, LitStr, parse_macro_input};

fn to_snake_case(s: &str) -> String {
    let mut out = String::new();
    for (i, ch) in s.char_indices() {
        if ch.is_uppercase() {
            if i != 0 {
                out.push('_');
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

#[proc_macro_derive(DomainEvent, attributes(event, event_name))]
pub fn derive_domain_event(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let mut prefix: Option<String> = None;
    for attr in &input.attrs {
        if attr.path().is_ident("event") {
            let _ = attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("prefix") {
                    let value: LitStr = meta.value()?.parse()?;
                    prefix = Some(value.value());
                }
                Ok(())
            });
        }
    }

    let data = match &input.data {
        Data::Enum(d) => d,
        _ => panic!(
            "DomainEvent can only be derived for enums (your event set is one enum per aggregate)"
        ),
    };

    let arms = data.variants.iter().map(|variant| {
        let vident = &variant.ident;

        let mut explicit: Option<String> = None;
        for attr in &variant.attrs {
            if attr.path().is_ident("event_name") {
                if let Ok(value) = attr.parse_args::<LitStr>() {
                    explicit = Some(value.value());
                }
            }
        }

        let type_str = explicit.unwrap_or_else(|| {
            let snake = to_snake_case(&vident.to_string());
            match &prefix {
                Some(p) => format!("{p}.{snake}"),
                None => snake,
            }
        });

        let pattern = match &variant.fields {
            Fields::Unit => quote! { #name::#vident },
            Fields::Unnamed(_) => quote! { #name::#vident(..) },
            Fields::Named(_) => quote! { #name::#vident { .. } },
        };

        quote! { #pattern => #type_str }
    });

    quote! {
        impl DomainEvent for #name {
            fn event_name(&self) -> &'static str {
                match self {
                    #(#arms),*
                }
            }
        }
    }
    .into()
}

#[proc_macro_derive(AggregateMeta, attributes(aggregate, aggregate_id, version))]
pub fn derive_aggregate_meta(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let mut type_override: Option<String> = None;
    for attr in &input.attrs {
        if attr.path().is_ident("aggregate") {
            let _ = attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("type") {
                    let value: LitStr = meta.value()?.parse()?;
                    type_override = Some(value.value());
                }
                Ok(())
            });
        }
    }
    let domain_name = type_override.unwrap_or_else(|| to_snake_case(&name.to_string()));

    let fields = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(f) => &f.named,
            _ => panic!("AggregateMeta requires named struct fields"),
        },
        _ => panic!("AggregateMeta can only be derived for structs"),
    };

    let mut id_field = None;
    let mut seq_field = None;
    for field in fields {
        let fname = field.ident.as_ref().unwrap();
        let has_id_attr = field
            .attrs
            .iter()
            .any(|a| a.path().is_ident("aggregate_id"));
        let has_seq_attr = field.attrs.iter().any(|a| a.path().is_ident("version"));

        if has_id_attr {
            id_field = Some(fname.clone());
        } else if id_field.is_none() && fname == "id" {
            id_field = Some(fname.clone()); // convention fallback
        }

        if has_seq_attr {
            seq_field = Some(fname.clone());
        } else if seq_field.is_none() && (fname == "version" || fname == "ver") {
            seq_field = Some(fname.clone()); // convention fallback
        }
    }

    let id_field = id_field.expect("AggregateMeta: tag a field #[aggregate_id] or name it `id`");
    let seq_field =
        seq_field.expect("AggregateMeta: tag a field #[version] or name it `version`/`seq`");

    quote! {
        impl AggregateMeta for #name {
            fn domain_name() -> &'static str { #domain_name }
            fn id(&self) -> &str { &self.#id_field }
            fn version(&self) -> u64 { self.#seq_field }
        }
    }
    .into()
}

#[proc_macro_derive(ProjectorMeta, attributes(projector))]
pub fn derive_projector_meta(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let mut name_override: Option<String> = None;
    for attr in &input.attrs {
        if attr.path().is_ident("projector") {
            let _ = attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("name") {
                    let value: LitStr = meta.value()?.parse()?;
                    name_override = Some(value.value());
                }
                Ok(())
            });
        }
    }
    let proj_name = name_override.unwrap_or_else(|| to_snake_case(&name.to_string()));

    quote! {
        impl ProjectorMeta for #name {
            fn name(&self) -> &'static str { #proj_name }
        }
    }
    .into()
}
