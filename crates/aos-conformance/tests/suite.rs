#[test]
fn log_chain() {
    aos_conformance::log_chain();
}

#[test]
fn transactions() {
    aos_conformance::transactions_property();
}

#[test]
fn revert() {
    aos_conformance::revert_property();
}

#[test]
fn branch() {
    aos_conformance::branch_rules();
}

#[test]
fn capability() {
    aos_conformance::capability_rules();
}

#[test]
fn policy() {
    aos_conformance::policy_priority();
}

#[cfg(feature = "dev-confirmer")]
#[test]
fn egress() {
    aos_conformance::egress_gate();
}

#[test]
fn taint() {
    aos_conformance::taint_and_recipient();
}

#[test]
fn actions() {
    aos_conformance::action_sandbox_checks();
}

#[test]
fn query() {
    aos_conformance::query_page_and_refs();
}

#[test]
fn schemas() {
    aos_conformance::schemas_match_builtins();
}

#[test]
fn human_edit() {
    aos_conformance::human_edit_uses_sys_action();
}

#[cfg(feature = "dev-confirmer")]
#[test]
fn vectors() {
    aos_conformance::committed_vectors();
}
