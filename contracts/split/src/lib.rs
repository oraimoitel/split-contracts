//! StellarSplit — on-chain invoice & payment splitting contract.
//!
//! Allows a creator to define an invoice with multiple recipients and amounts.
//! Payers contribute funds; once fully funded the contract auto-routes tokens to
//! each recipient. If the deadline passes unfunded, payers are refunded.

#![no_std]

mod events;
mod types;

#[cfg(test)]
mod test;

use soroban_sdk::{contract, contractimpl, symbol_short, token, Address, Env, Symbol, Vec};
use types::{AuditEntry, Invoice, InvoiceStatus, InvoiceV1, Payment};

// ---------------------------------------------------------------------------
// Storage key helpers
// ---------------------------------------------------------------------------

fn counter_key() -> Symbol {
    symbol_short!("counter")
}

fn invoice_key(id: u64) -> (Symbol, u64) {
    (symbol_short!("inv"), id)
}

fn admin_key() -> Symbol {
    symbol_short!("admin")
}

fn paused_key() -> Symbol {
    symbol_short!("paused")
}

fn audit_log_key(id: u64) -> (Symbol, u64) {
    (symbol_short!("log"), id)
}

// ---------------------------------------------------------------------------
// Storage helpers
// ---------------------------------------------------------------------------

fn load_invoice(env: &Env, id: u64) -> Invoice {
    env.storage()
        .persistent()
        .get(&invoice_key(id))
        .expect("invoice not found")
}

fn save_invoice(env: &Env, id: u64, invoice: &Invoice) {
    env.storage().persistent().set(&invoice_key(id), invoice);
}

fn append_audit_entry(env: &Env, id: u64, action: Symbol, actor: &Address) {
    let timestamp = env.ledger().timestamp();
    let entry = AuditEntry { action, actor: actor.clone(), timestamp };
    let mut log: Vec<AuditEntry> = env
        .storage()
        .persistent()
        .get(&audit_log_key(id))
        .unwrap_or_else(|| Vec::new(env));
    log.push_back(entry);
    env.storage().persistent().set(&audit_log_key(id), &log);
}

fn is_paused(env: &Env) -> bool {
    env.storage()
        .persistent()
        .get(&paused_key())
        .unwrap_or(false)
}

fn require_not_paused(env: &Env) {
    assert!(!is_paused(env), "contract is paused");
}

fn require_admin(env: &Env) -> Address {
    let admin: Address = env
        .storage()
        .instance()
        .get(&admin_key())
        .expect("admin not set");
    admin.require_auth();
    admin
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct SplitContract;

#[contractimpl]
impl SplitContract {
    /// Set the contract admin. Can only be called once.
    pub fn initialize(env: Env, admin: Address) {
        assert!(
            !env.storage().instance().has(&admin_key()),
            "already initialized"
        );
        env.storage().instance().set(&admin_key(), &admin);
    }

    /// Pause the contract. Requires admin auth.
    pub fn pause(env: Env, admin: Address) {
        require_admin(&env);
        let _ = admin;
        env.storage().persistent().set(&paused_key(), &true);
    }

    /// Unpause the contract. Requires admin auth.
    pub fn unpause(env: Env, admin: Address) {
        require_admin(&env);
        let _ = admin;
        env.storage().persistent().set(&paused_key(), &false);
    }

    /// Create a new invoice.
    ///
    /// # Arguments
    /// * `creator`        – address that owns the invoice (must authorise)
    /// * `recipients`     – ordered list of recipient addresses
    /// * `amounts`        – amount owed to each recipient (parallel to `recipients`)
    /// * `token`          – token contract address
    /// * `deadline`       – Unix timestamp; after this refunds become available
    /// * `allowed_payers` – optional whitelist; when `Some`, only listed addresses may pay
    ///
    /// # Returns
    /// The new invoice ID (monotonically increasing u64).
    pub fn create_invoice(
        env: Env,
        creator: Address,
        recipients: Vec<Address>,
        amounts: Vec<i128>,
        token: Address,
        deadline: u64,
        allowed_payers: Option<Vec<Address>>,
    ) -> u64 {
        require_not_paused(&env);
        creator.require_auth();

        assert!(
            recipients.len() == amounts.len(),
            "recipients and amounts length mismatch"
        );
        assert!(!recipients.is_empty(), "must have at least one recipient");
        assert!(
            deadline > env.ledger().timestamp(),
            "deadline must be in the future"
        );
        for amt in amounts.iter() {
            assert!(amt > 0, "amounts must be positive");
        }

        let id: u64 = env
            .storage()
            .persistent()
            .get(&counter_key())
            .unwrap_or(0u64)
            + 1;
        env.storage().persistent().set(&counter_key(), &id);

        let invoice = Invoice {
            creator: creator.clone(),
            recipients,
            amounts,
            token,
            deadline,
            funded: 0,
            status: InvoiceStatus::Pending,
            payments: Vec::new(&env),
            allowed_payers,
        };

        save_invoice(&env, id, &invoice);

        let total: i128 = invoice.amounts.iter().sum();
        events::invoice_created(&env, id, &creator, total);

        id
    }

    /// Pay toward an invoice.
    ///
    /// If the invoice has an `allowed_payers` whitelist, panics with
    /// "payer not allowed" when `payer` is not in the list.
    pub fn pay(env: Env, payer: Address, invoice_id: u64, amount: i128) {
        require_not_paused(&env);
        payer.require_auth();

        let mut invoice = load_invoice(&env, invoice_id);

        assert!(
            invoice.status == InvoiceStatus::Pending,
            "invoice is not pending"
        );
        assert!(
            env.ledger().timestamp() <= invoice.deadline,
            "invoice deadline has passed"
        );
        assert!(amount > 0, "payment amount must be positive");

        // Whitelist check — issue #30.
        if let Some(ref whitelist) = invoice.allowed_payers {
            assert!(whitelist.contains(&payer), "payer not allowed");
        }

        let total: i128 = invoice.amounts.iter().sum();
        let remaining = total - invoice.funded;
        assert!(amount <= remaining, "payment exceeds remaining balance");

        let token_client = token::Client::new(&env, &invoice.token);
        token_client.transfer(&payer, &env.current_contract_address(), &amount);

        invoice.payments.push_back(Payment { payer: payer.clone(), amount });
        invoice.funded += amount;

        append_audit_entry(&env, invoice_id, symbol_short!("pay"), &payer);
        events::payment_received(&env, invoice_id, &payer, amount);

        if invoice.funded >= total {
            Self::_release(&env, invoice_id, &mut invoice, &payer);
        } else {
            save_invoice(&env, invoice_id, &invoice);
        }
    }

    /// Release funds to all recipients once the invoice is fully funded.
    pub fn release(env: Env, invoice_id: u64) {
        require_not_paused(&env);
        let mut invoice = load_invoice(&env, invoice_id);

        assert!(
            invoice.status == InvoiceStatus::Pending,
            "invoice is not pending"
        );

        let total: i128 = invoice.amounts.iter().sum();
        assert!(invoice.funded >= total, "invoice not fully funded");

        let caller = env.current_contract_address();
        Self::_release(&env, invoice_id, &mut invoice, &caller);
    }

    /// Refund all payers if the deadline has passed and the invoice is not fully funded.
    pub fn refund(env: Env, invoice_id: u64) {
        require_not_paused(&env);
        let mut invoice = load_invoice(&env, invoice_id);

        assert!(
            invoice.status == InvoiceStatus::Pending,
            "invoice is not pending"
        );
        assert!(
            env.ledger().timestamp() > invoice.deadline,
            "deadline has not passed"
        );

        let token_client = token::Client::new(&env, &invoice.token);

        for payment in invoice.payments.iter() {
            token_client.transfer(
                &env.current_contract_address(),
                &payment.payer,
                &payment.amount,
            );
            events::payer_refunded(&env, invoice_id, &payment.payer, payment.amount);
        }

        invoice.status = InvoiceStatus::Refunded;
        save_invoice(&env, invoice_id, &invoice);

        let actor = env.current_contract_address();
        append_audit_entry(&env, invoice_id, symbol_short!("refund"), &actor);
        events::invoice_refunded(&env, invoice_id);
    }

    /// Cancel an invoice before any payments are made.
    pub fn cancel_invoice(env: Env, caller: Address, invoice_id: u64) {
        require_not_paused(&env);
        caller.require_auth();

        let mut invoice = load_invoice(&env, invoice_id);

        assert!(
            invoice.status == InvoiceStatus::Pending,
            "invoice is not pending"
        );
        assert!(invoice.creator == caller, "only creator can cancel");
        assert!(invoice.funded == 0, "cannot cancel invoice with payments");

        invoice.status = InvoiceStatus::Cancelled;
        save_invoice(&env, invoice_id, &invoice);
        append_audit_entry(&env, invoice_id, symbol_short!("cancel"), &caller);
    }

    /// Transfer invoice ownership to a new creator.
    pub fn transfer_invoice(env: Env, invoice_id: u64, new_creator: Address) {
        require_not_paused(&env);
        let mut invoice = load_invoice(&env, invoice_id);

        assert!(
            invoice.status == InvoiceStatus::Pending,
            "invoice is not pending"
        );

        invoice.creator.require_auth();
        invoice.creator = new_creator;
        save_invoice(&env, invoice_id, &invoice);
    }

    /// Extend the deadline for an invoice.
    pub fn extend_deadline(env: Env, caller: Address, invoice_id: u64, new_deadline: u64) {
        require_not_paused(&env);
        caller.require_auth();

        let mut invoice = load_invoice(&env, invoice_id);

        assert!(
            invoice.status == InvoiceStatus::Pending,
            "invoice is not pending"
        );
        assert!(invoice.creator == caller, "only creator can extend deadline");
        assert!(
            new_deadline > env.ledger().timestamp(),
            "new deadline must be in the future"
        );

        invoice.deadline = new_deadline;
        save_invoice(&env, invoice_id, &invoice);
        append_audit_entry(&env, invoice_id, symbol_short!("extend"), &caller);
    }

    // -----------------------------------------------------------------------
    // #31 — Storage schema migration v2
    // -----------------------------------------------------------------------

    /// Migrate an invoice stored under the v1 schema to v2.
    ///
    /// Reads the invoice using `InvoiceV1` (schema without `allowed_payers`),
    /// then re-saves it as `Invoice` (v2) with `allowed_payers` defaulting to `None`.
    /// Requires admin auth.
    pub fn migrate_invoice(env: Env, invoice_id: u64) {
        require_admin(&env);

        let v1: InvoiceV1 = env
            .storage()
            .persistent()
            .get(&invoice_key(invoice_id))
            .expect("invoice not found");

        let v2 = Invoice {
            creator: v1.creator,
            recipients: v1.recipients,
            amounts: v1.amounts,
            token: v1.token,
            deadline: v1.deadline,
            funded: v1.funded,
            status: v1.status,
            payments: v1.payments,
            allowed_payers: None,
        };

        save_invoice(&env, invoice_id, &v2);
    }

    // -----------------------------------------------------------------------
    // Read-only
    // -----------------------------------------------------------------------

    pub fn get_invoice(env: Env, invoice_id: u64) -> Invoice {
        load_invoice(&env, invoice_id)
    }

    pub fn get_audit_log(env: Env, invoice_id: u64) -> Vec<AuditEntry> {
        env.storage()
            .persistent()
            .get(&audit_log_key(invoice_id))
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Return the total amount contributed by `payer` toward `invoice_id`.
    pub fn get_payer_total(env: Env, invoice_id: u64, payer: Address) -> i128 {
        let invoice = load_invoice(&env, invoice_id);
        invoice
            .payments
            .iter()
            .filter(|p| p.payer == payer)
            .map(|p| p.amount)
            .sum()
    }

    /// Returns true if the invoice exists and its status matches `expected_status`.
    pub fn verify_invoice(env: Env, invoice_id: u64, expected_status: InvoiceStatus) -> bool {
        match env
            .storage()
            .persistent()
            .get::<(Symbol, u64), Invoice>(&invoice_key(invoice_id))
        {
            Some(invoice) => invoice.status == expected_status,
            None => false,
        }
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn _release(env: &Env, invoice_id: u64, invoice: &mut Invoice, actor: &Address) {
        let token_client = token::Client::new(env, &invoice.token);

        for (recipient, amount) in invoice.recipients.iter().zip(invoice.amounts.iter()) {
            token_client.transfer(&env.current_contract_address(), &recipient, &amount);
        }

        invoice.status = InvoiceStatus::Released;
        save_invoice(env, invoice_id, invoice);
        append_audit_entry(env, invoice_id, symbol_short!("release"), actor);
        events::invoice_released(env, invoice_id, &invoice.recipients);
    }
}
