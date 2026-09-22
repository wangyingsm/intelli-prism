//! A workflow over a carrier that is not a database, to show the macro cares about order
//! rather than about sqlx.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use typestate_txn::transaction;

/// Stands in for a transaction: it records what was written, whether it landed, and, where
/// the test can still see it, whether it was undone.
#[derive(Default)]
pub struct Ledger {
    written: Vec<String>,
    landed: bool,
    undone: Arc<AtomicBool>,
}

impl Ledger {
    fn write(&mut self, entry: &str) {
        self.written.push(entry.to_owned());
    }

    fn land(mut self) -> Self {
        self.landed = true;
        self
    }
}

/// What a finished order hands back.
#[derive(Debug, PartialEq)]
pub struct Placed {
    basket: String,
    paid: u32,
}

/// Every way placing an order can fail.
#[derive(Debug, PartialEq)]
pub struct OrderError(String);

transaction! {
    name: Order,
    generics: <>,
    carrier: Ledger,
    error: OrderError,
    record: Placed,
    finish: { let _ = carrier.land(); },
    abort: { carrier.undone.store(true, Ordering::SeqCst); },
    steps: {
        fill(item: &str) -> basket: String as Filled {
            if item.is_empty() {
                return Err(OrderError("nothing to put in the basket".to_owned()));
            }
            carrier.write(item);
            item.to_owned()
        }
        pay(amount: u32) -> paid: u32 as Paid {
            carrier.write(&format!("paid {amount} for {basket}"));
            amount
        }
    }
}

#[tokio::test]
async fn the_steps_run_in_their_order() {
    let placed = OrderTxn::new(Ledger::default())
        .fill("apples")
        .await
        .unwrap()
        .pay(3)
        .await
        .unwrap()
        .commit()
        .await
        .unwrap();
    assert_eq!(
        placed,
        Placed {
            basket: "apples".to_owned(),
            paid: 3
        }
    );
}

#[tokio::test]
async fn a_step_carries_what_the_step_before_it_produced() {
    let filled = OrderTxn::new(Ledger::default())
        .fill("pears")
        .await
        .unwrap();
    assert_eq!(filled.basket(), "pears");
    let paid = filled.pay(7).await.unwrap();
    assert_eq!(paid.basket(), "pears");
    assert_eq!(paid.paid(), &7);
}

#[tokio::test]
async fn a_step_that_fails_undoes_the_carrier_before_it_answers() {
    let undone = Arc::new(AtomicBool::new(false));
    let ledger = Ledger {
        undone: undone.clone(),
        ..Ledger::default()
    };
    let refused = OrderTxn::new(ledger).fill("").await;
    assert_eq!(
        refused.err(),
        Some(OrderError("nothing to put in the basket".to_owned()))
    );
    assert!(undone.load(Ordering::SeqCst));
}

#[tokio::test]
async fn a_transaction_that_lands_is_never_undone() {
    let undone = Arc::new(AtomicBool::new(false));
    let ledger = Ledger {
        undone: undone.clone(),
        ..Ledger::default()
    };
    OrderTxn::new(ledger)
        .fill("apples")
        .await
        .unwrap()
        .pay(3)
        .await
        .unwrap()
        .commit()
        .await
        .unwrap();
    assert!(!undone.load(Ordering::SeqCst));
}
