//! A macro for transactions whose steps run in one order, fixed at compile time.
//!
//! The order of database writes matters, and a commit is only legal once everything before
//! it has happened. Checking that at runtime means every caller can get it wrong. This macro
//! writes the state machine that makes the wrong order fail to compile.
//!
//! It generates the plumbing only: the stages, the transitions, the accessors and the commit.
//! What each step actually writes stays where it belongs, in the body the caller supplies.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Block, Generics, Ident, Token, Type, braced, parenthesized};

mod keyword {
    syn::custom_keyword!(name);
    syn::custom_keyword!(generics);
    syn::custom_keyword!(carrier);
    syn::custom_keyword!(error);
    syn::custom_keyword!(record);
    syn::custom_keyword!(finish);
    syn::custom_keyword!(abort);
    syn::custom_keyword!(steps);
}

/// One input a step takes from its caller.
struct Input {
    name: Ident,
    ty: Type,
}

impl Parse for Input {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let name = input.parse()?;
        input.parse::<Token![:]>()?;
        let ty = input.parse()?;
        Ok(Self { name, ty })
    }
}

/// One step: what it takes, what it produces, the stage it lands in, and what it does.
struct Step {
    name: Ident,
    inputs: Punctuated<Input, Token![,]>,
    output: Ident,
    output_type: Type,
    stage: Ident,
    body: Block,
}

impl Parse for Step {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let name = input.parse()?;
        let arguments;
        parenthesized!(arguments in input);
        let inputs = arguments.parse_terminated(Input::parse, Token![,])?;
        input.parse::<Token![->]>()?;
        let output = input.parse()?;
        input.parse::<Token![:]>()?;
        let output_type = input.parse()?;
        input.parse::<Token![as]>()?;
        let stage = input.parse()?;
        let body = input.parse()?;
        Ok(Self {
            name,
            inputs,
            output,
            output_type,
            stage,
            body,
        })
    }
}

/// A whole workflow, as the macro is given it.
struct Workflow {
    name: Ident,
    generics: Generics,
    carrier: Type,
    error: Type,
    record: Type,
    finish: Block,
    abort: Block,
    steps: Vec<Step>,
}

impl Parse for Workflow {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        input.parse::<keyword::name>()?;
        input.parse::<Token![:]>()?;
        let name: Ident = input.parse()?;
        input.parse::<Token![,]>()?;

        input.parse::<keyword::generics>()?;
        input.parse::<Token![:]>()?;
        let generics: Generics = input.parse()?;
        input.parse::<Token![,]>()?;

        input.parse::<keyword::carrier>()?;
        input.parse::<Token![:]>()?;
        let carrier: Type = input.parse()?;
        input.parse::<Token![,]>()?;

        input.parse::<keyword::error>()?;
        input.parse::<Token![:]>()?;
        let error: Type = input.parse()?;
        input.parse::<Token![,]>()?;

        input.parse::<keyword::record>()?;
        input.parse::<Token![:]>()?;
        let record: Type = input.parse()?;
        input.parse::<Token![,]>()?;

        input.parse::<keyword::finish>()?;
        input.parse::<Token![:]>()?;
        let finish: Block = input.parse()?;
        input.parse::<Token![,]>()?;

        input.parse::<keyword::abort>()?;
        input.parse::<Token![:]>()?;
        let abort: Block = input.parse()?;
        input.parse::<Token![,]>()?;

        input.parse::<keyword::steps>()?;
        input.parse::<Token![:]>()?;
        let listed;
        braced!(listed in input);
        let mut steps = Vec::new();
        while !listed.is_empty() {
            steps.push(listed.parse()?);
        }
        if steps.is_empty() {
            return Err(syn::Error::new(
                name.span(),
                "a transaction needs at least one step, or nothing can be committed",
            ));
        }
        Ok(Self {
            name,
            generics,
            carrier,
            error,
            record,
            finish,
            abort,
            steps,
        })
    }
}

/// Writes a transaction whose steps run in one order.
///
/// Every generated name starts with `name`, so two workflows never collide. Each step lands
/// in its own stage, and the step after it is implemented only on that stage: calling them
/// out of order, or committing early, does not compile.
///
/// Inside a step body, `carrier` is `&mut` the carrier, the step's inputs are in scope by
/// name, and so is every value an earlier step produced. The body evaluates to this step's
/// output, and `?` in it fails the step. Inside `finish`, `carrier` is the carrier itself, by
/// value, since committing it usually consumes it.
///
/// `abort` runs the moment a step fails, with `carrier` by value, before the error is handed
/// back. It is where the carrier is undone at once, rather than whenever dropping it gets
/// round to it: a database transaction left to its drop keeps its locks until the pool next
/// hands its connection out.
///
/// ```
/// use typestate_txn::transaction;
///
/// pub struct Ledger;
/// pub struct Placed { basket: String, paid: u32 }
/// #[derive(Debug)] pub struct OrderError;
///
/// transaction! {
///     name: Order,
///     generics: <>,
///     carrier: Ledger,
///     error: OrderError,
///     record: Placed,
///     finish: { let Ledger = carrier; },
///     abort: { let Ledger = carrier; },
///     steps: {
///         fill(item: &str) -> basket: String as Filled {
///             let _ = &carrier;
///             item.to_owned()
///         }
///         pay(amount: u32) -> paid: u32 as Paid {
///             let _ = (&carrier, &basket);
///             amount
///         }
///     }
/// }
///
/// # async fn run() {
/// let placed = OrderTxn::new(Ledger)
///     .fill("apples").await.unwrap()
///     .pay(3).await.unwrap()
///     .commit().await.unwrap();
/// # }
/// # fn main() {}
/// ```
///
/// A step that fails hands its carrier to `abort` before the error comes back:
///
/// ```
/// use std::sync::Arc;
/// use std::sync::atomic::{AtomicBool, Ordering};
///
/// use typestate_txn::transaction;
///
/// pub struct Ledger { undone: Arc<AtomicBool> }
/// pub struct Placed { basket: String }
/// #[derive(Debug)] pub struct OrderError;
///
/// transaction! {
///     name: Order,
///     generics: <>,
///     carrier: Ledger,
///     error: OrderError,
///     record: Placed,
///     finish: { let _ = carrier; },
///     abort: { carrier.undone.store(true, Ordering::SeqCst); },
///     steps: {
///         fill(item: &str) -> basket: String as Filled {
///             let _ = &carrier;
///             if item.is_empty() {
///                 return Err(OrderError);
///             }
///             item.to_owned()
///         }
///     }
/// }
///
/// # async fn run() {
/// let undone = Arc::new(AtomicBool::new(false));
/// let failed = OrderTxn::new(Ledger { undone: undone.clone() }).fill("").await;
/// assert!(failed.is_err());
/// assert!(undone.load(Ordering::SeqCst));
///
/// let kept = Arc::new(AtomicBool::new(false));
/// let placed = OrderTxn::new(Ledger { undone: kept.clone() })
///     .fill("apples").await.unwrap()
///     .commit().await.unwrap();
/// assert_eq!(placed.basket, "apples");
/// assert!(!kept.load(Ordering::SeqCst));
/// # }
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() { run().await }
/// ```
///
/// Paying before there is anything to pay for does not compile:
///
/// ```compile_fail
/// use typestate_txn::transaction;
///
/// pub struct Ledger;
/// pub struct Placed { basket: String, paid: u32 }
/// #[derive(Debug)] pub struct OrderError;
///
/// transaction! {
///     name: Order,
///     generics: <>,
///     carrier: Ledger,
///     error: OrderError,
///     record: Placed,
///     finish: { let Ledger = carrier; },
///     abort: { let Ledger = carrier; },
///     steps: {
///         fill(item: &str) -> basket: String as Filled {
///             let _ = &carrier;
///             item.to_owned()
///         }
///         pay(amount: u32) -> paid: u32 as Paid {
///             let _ = (&carrier, &basket);
///             amount
///         }
///     }
/// }
///
/// # async fn run() {
/// // pay belongs to OrderTxn<Filled>, so it cannot run on a transaction that has filled nothing.
/// let txn = OrderTxn::new(Ledger).pay(3).await.unwrap();
/// # }
/// # fn main() {}
/// ```
///
/// Committing before the last step does not compile either:
///
/// ```compile_fail
/// use typestate_txn::transaction;
///
/// pub struct Ledger;
/// pub struct Placed { basket: String, paid: u32 }
/// #[derive(Debug)] pub struct OrderError;
///
/// transaction! {
///     name: Order,
///     generics: <>,
///     carrier: Ledger,
///     error: OrderError,
///     record: Placed,
///     finish: { let Ledger = carrier; },
///     abort: { let Ledger = carrier; },
///     steps: {
///         fill(item: &str) -> basket: String as Filled {
///             let _ = &carrier;
///             item.to_owned()
///         }
///         pay(amount: u32) -> paid: u32 as Paid {
///             let _ = (&carrier, &basket);
///             amount
///         }
///     }
/// }
///
/// # async fn run() {
/// let placed = OrderTxn::new(Ledger).fill("apples").await.unwrap().commit().await.unwrap();
/// # }
/// # fn main() {}
/// ```
#[proc_macro]
pub fn transaction(input: TokenStream) -> TokenStream {
    let workflow = syn::parse_macro_input!(input as Workflow);
    expand(&workflow).into()
}

fn expand(workflow: &Workflow) -> proc_macro2::TokenStream {
    let Workflow {
        name,
        generics,
        carrier,
        error,
        record,
        finish,
        abort,
        steps,
    } = workflow;

    // The workflow's own parameters have to be spliced in beside the stage, not after it.
    let declared = &generics.params;
    let parameters: Vec<&syn::GenericParam> = declared.iter().collect();
    let where_clause = &generics.where_clause;
    let impl_head = match parameters.is_empty() {
        true => quote!(impl),
        false => quote!(impl<#declared>),
    };
    let type_arguments: Vec<proc_macro2::TokenStream> = generics
        .params
        .iter()
        .map(|parameter| match parameter {
            syn::GenericParam::Type(ty) => {
                let ident = &ty.ident;
                quote!(#ident)
            }
            syn::GenericParam::Lifetime(lifetime) => {
                let lifetime = &lifetime.lifetime;
                quote!(#lifetime)
            }
            syn::GenericParam::Const(constant) => {
                let ident = &constant.ident;
                quote!(#ident)
            }
        })
        .collect();
    let sealed = format_ident!("{}_sealed", to_snake(&name.to_string()));
    let stage_trait = format_ident!("{name}Stage");
    let begun = format_ident!("{name}Begun");
    let txn = format_ident!("{name}Txn");

    // Each stage carries everything the steps before it produced.
    let mut carried: Vec<(&Ident, &Type)> = Vec::new();
    let mut stage_names = vec![begun.clone()];
    let mut stages = vec![quote! {
        /// Nothing is written yet.
        pub struct #begun;
    }];
    let mut transitions = Vec::new();

    for step in steps {
        let previous_stage = stage_names
            .last()
            .expect("the first stage is pushed before the loop")
            .clone();
        let stage = format_ident!("{name}{}", step.stage);
        let Step {
            name: step_name,
            inputs,
            output,
            output_type,
            body,
            ..
        } = step;

        let previous_fields: Vec<&Ident> = carried.iter().map(|(field, _)| *field).collect();
        carried.push((output, output_type));
        let fields = carried.iter().map(|(field, ty)| quote!(#field: #ty));
        let doc = format!("Everything written up to and including `{step_name}`.");
        stages.push(quote! {
            #[doc = #doc]
            pub struct #stage {
                #(#fields),*
            }
        });

        let accessors = carried.iter().map(|(field, ty)| {
            let doc = format!("The `{field}` this transaction wrote.");
            quote! {
                #[doc = #doc]
                pub fn #field(&self) -> &#ty {
                    &self.stage.#field
                }
            }
        });

        let parameter_names = inputs.iter().map(|input| &input.name);
        let parameter_list = inputs.iter().map(|input| {
            let Input { name, ty } = input;
            quote!(#name: #ty)
        });
        let all_fields: Vec<&Ident> = carried.iter().map(|(field, _)| *field).collect();
        let doc =
            format!("Runs `{step_name}`, leaving the transaction able to run the step after it.");

        transitions.push(quote! {
            #impl_head #txn<#(#type_arguments,)* #previous_stage> #where_clause {
                #[doc = #doc]
                pub async fn #step_name(
                    self,
                    #(#parameter_list),*
                ) -> ::core::result::Result<#txn<#(#type_arguments,)* #stage>, #error> {
                    let #previous_stage { #(#previous_fields),* } = self.stage;
                    let mut carrier = self.carrier;
                    #(let _ = &#parameter_names;)*
                    let outcome: ::core::result::Result<#output_type, #error> = async {
                        let carrier = &mut carrier;
                        ::core::result::Result::Ok(#body)
                    }
                    .await;
                    match outcome {
                        ::core::result::Result::Ok(#output) => ::core::result::Result::Ok(#txn {
                            carrier,
                            stage: #stage { #(#all_fields),* },
                        }),
                        ::core::result::Result::Err(error) => {
                            #abort
                            ::core::result::Result::Err(error)
                        }
                    }
                }
            }

            #impl_head #txn<#(#type_arguments,)* #stage> #where_clause {
                #(#accessors)*
            }
        });

        stage_names.push(stage);
    }

    let last_stage = stage_names
        .last()
        .expect("the first stage is pushed before the loop");
    let committed_fields: Vec<&Ident> = carried.iter().map(|(field, _)| *field).collect();
    let stage_list = &stage_names;
    let struct_doc = format!(
        "`{name}`, whose steps run in one order. Each step is implemented only on the stage \
         before it, so a step cannot be skipped or reordered: the call that would do so does \
         not compile. Dropping the transaction before committing leaves the carrier to undo \
         whatever it wrote."
    );

    quote! {
        mod #sealed {
            pub trait Sealed {}
        }

        #[doc = "How far this transaction has got. Only the stages below are ones."]
        pub trait #stage_trait: #sealed::Sealed {}

        #(#stages)*

        #(
            impl #sealed::Sealed for #stage_list {}
            impl #stage_trait for #stage_list {}
        )*

        #[doc = #struct_doc]
        pub struct #txn<#(#parameters,)* S: #stage_trait> #where_clause {
            carrier: #carrier,
            stage: S,
        }

        #impl_head #txn<#(#type_arguments,)* #begun> #where_clause {
            #[doc = "Takes over a carrier that has already been opened."]
            pub fn new(carrier: #carrier) -> Self {
                Self { carrier, stage: #begun }
            }
        }

        #(#transitions)*

        #impl_head #txn<#(#type_arguments,)* #last_stage> #where_clause {
            #[doc = "Lands everything this transaction wrote, together."]
            pub async fn commit(self) -> ::core::result::Result<#record, #error> {
                let #last_stage { #(#committed_fields),* } = self.stage;
                let carrier = self.carrier;
                #finish;
                ::core::result::Result::Ok(#record { #(#committed_fields),* })
            }
        }
    }
}

/// `UserCreate` as `user_create`, for the module a workflow's sealed marker hides in.
fn to_snake(camel: &str) -> String {
    let mut snake = String::with_capacity(camel.len() + 4);
    for (at, character) in camel.char_indices() {
        if character.is_uppercase() {
            if at != 0 {
                snake.push('_');
            }
            snake.extend(character.to_lowercase());
        } else {
            snake.push(character);
        }
    }
    snake
}
