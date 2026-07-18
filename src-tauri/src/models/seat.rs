//! Wave 3.1 — model **seats** (PLAN §4). A seat is a USER-DEFINED name (e.g.
//! "Coding", "Reviewer") that an agent/tool/skill references INSTEAD of a
//! hardcoded provider+model, so rebinding the seat changes behavior with no code
//! change. Bindings are **per-profile** (a profile can point a seat at a
//! different — e.g. forced-local — model than another profile); they live in the
//! profile's `seat_bindings` table (`ProfileDb::{set,get,list,delete}_seat_binding`).
//!
//! [`resolve_seat`] turns a seat name into a concrete `(provider_id, model)`.
//! It is a **preference resolver only** — it never touches privacy. The pair it
//! returns is a *candidate*: the per-turn privacy gate + `enforce_local_routing`
//! still get the final say downstream, so a seat may PREFER a cloud model but can
//! never defeat a `RouteLocal` / `LocalRequired` verdict (a must-stay-local turn
//! is rerouted to a local provider exactly as an explicit cloud pick would be).
//!
//! An empty seat, the literal `"inherit"`, an unbound seat, or a binding whose
//! provider has since been deleted all fall back to the caller's own model — so
//! resolution NEVER yields an unusable pair. (This `inherit` default is what
//! pre-3.1 callers and not-yet-bound personas get.)

use crate::models::ModelManager;
use crate::storage::Storage;

/// Resolve a seat to a concrete `(provider_id, model)` for `profile`, falling
/// back to the caller's own `(provider, model)` when the seat is empty/`inherit`,
/// unbound, unreadable, or bound to a provider that no longer exists.
pub fn resolve_seat(
    storage: &Storage,
    model_manager: &ModelManager,
    profile: &str,
    seat: &str,
    caller_provider_id: &str,
    caller_model: &str,
) -> (String, String) {
    let inherit = || (caller_provider_id.to_string(), caller_model.to_string());
    let seat = seat.trim();
    if seat.is_empty() || seat.eq_ignore_ascii_case("inherit") {
        return inherit();
    }
    match storage
        .open_profile(profile)
        .ok()
        .and_then(|db| db.get_seat_binding(seat).ok().flatten())
    {
        // A live binding: use it — but only if its provider still exists.
        Some(b) if model_manager.get_provider(&b.provider_id).is_some() => (b.provider_id, b.model),
        // Unbound, unreadable, or a dangling provider id → inherit.
        _ => inherit(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Provider, ProviderKind};
    use std::sync::Arc;

    fn setup() -> (Arc<Storage>, ModelManager, std::path::PathBuf) {
        let mut root = std::env::temp_dir();
        root.push(format!("lhp-seat-{}", uuid::Uuid::new_v4()));
        let storage = Arc::new(Storage::open(&root).unwrap());
        let mm = ModelManager::new();
        mm.add_provider(Provider::new(
            "lmstudio",
            "LM Studio",
            "http://localhost:1234/v1",
            None,
            ProviderKind::Local,
        ));
        (storage, mm, root)
    }

    #[test]
    fn unbound_seat_inherits_the_callers_model() {
        let (storage, mm, root) = setup();
        let (p, m) = resolve_seat(&storage, &mm, "personal", "Coding", "cloudco", "gpt-x");
        assert_eq!((p.as_str(), m.as_str()), ("cloudco", "gpt-x"), "unbound → inherit");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn empty_and_inherit_seat_names_inherit() {
        let (storage, mm, root) = setup();
        assert_eq!(
            resolve_seat(&storage, &mm, "personal", "", "cloudco", "gpt-x"),
            ("cloudco".to_string(), "gpt-x".to_string())
        );
        assert_eq!(
            resolve_seat(&storage, &mm, "personal", "  inherit  ", "cloudco", "gpt-x"),
            ("cloudco".to_string(), "gpt-x".to_string())
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_bound_seat_resolves_to_its_pair_and_rebinding_changes_it() {
        let (storage, mm, root) = setup();
        let db = storage.open_profile("personal").unwrap();
        db.set_seat_binding("Coding", "lmstudio", "qwen3-14b").unwrap();

        let (p, m) = resolve_seat(&storage, &mm, "personal", "Coding", "cloudco", "gpt-x");
        assert_eq!((p.as_str(), m.as_str()), ("lmstudio", "qwen3-14b"), "bound seat used");

        // "rebinding a seat changes behavior with no code change" (manifest done-when).
        db.set_seat_binding("Coding", "lmstudio", "qwen3-30b").unwrap();
        let (_, m2) = resolve_seat(&storage, &mm, "personal", "Coding", "cloudco", "gpt-x");
        assert_eq!(m2, "qwen3-30b", "rebind changes what the seat resolves to");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_binding_to_a_deleted_provider_falls_back_to_inherit() {
        let (storage, mm, root) = setup();
        let db = storage.open_profile("personal").unwrap();
        // Bind to a provider id that is NOT registered in the ModelManager.
        db.set_seat_binding("Coding", "ghost-provider", "some-model").unwrap();
        let (p, m) = resolve_seat(&storage, &mm, "personal", "Coding", "cloudco", "gpt-x");
        assert_eq!(
            (p.as_str(), m.as_str()),
            ("cloudco", "gpt-x"),
            "a dangling provider id must not yield an unusable pair"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn bindings_are_per_profile() {
        let (storage, mm, root) = setup();
        storage
            .open_profile("work")
            .unwrap()
            .set_seat_binding("Coding", "lmstudio", "local-only")
            .unwrap();
        // "work" has the binding; "personal" does not → inherits.
        assert_eq!(
            resolve_seat(&storage, &mm, "work", "Coding", "cloudco", "gpt-x").1,
            "local-only"
        );
        assert_eq!(
            resolve_seat(&storage, &mm, "personal", "Coding", "cloudco", "gpt-x"),
            ("cloudco".to_string(), "gpt-x".to_string()),
            "another profile's binding does not leak across profiles"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
