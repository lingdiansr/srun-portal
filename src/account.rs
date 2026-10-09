//! Portal protocol and credential identities for one configured account.

/// One account represented both as the protocol's local/domain pair and as the
/// opaque, fully qualified key used by credential stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AccountIdentity {
    local: String,
    domain: String,
    qualified: String,
}

impl AccountIdentity {
    /// Parse the first embedded `@domain`, falling back to the legacy separate
    /// domain only when the username has no non-empty embedded suffix.
    pub(crate) fn parse(username: &str, legacy_domain: Option<&str>) -> Self {
        let (local, domain) = match username.split_once('@') {
            Some((local, domain)) if !domain.is_empty() => {
                (local.to_string(), format!("@{domain}"))
            }
            _ => (username.to_string(), normalize_domain(legacy_domain)),
        };
        let qualified = format!("{local}{domain}");
        Self {
            local,
            domain,
            qualified,
        }
    }

    pub(crate) fn local(&self) -> &str {
        &self.local
    }

    pub(crate) fn domain(&self) -> &str {
        &self.domain
    }

    pub(crate) fn qualified(&self) -> &str {
        &self.qualified
    }
}

fn normalize_domain(domain: Option<&str>) -> String {
    match domain {
        Some(domain) if !domain.is_empty() && domain.starts_with('@') => domain.to_string(),
        Some(domain) if !domain.is_empty() => format!("@{domain}"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_domains_make_distinct_qualified_identities() {
        let staff = AccountIdentity::parse("alice@staff", None);
        let student = AccountIdentity::parse("alice@student", None);

        assert_eq!(staff.local(), "alice");
        assert_eq!(student.local(), "alice");
        assert_eq!(staff.domain(), "@staff");
        assert_eq!(student.domain(), "@student");
        assert_eq!(staff.qualified(), "alice@staff");
        assert_eq!(student.qualified(), "alice@student");
        assert_ne!(staff.qualified(), student.qualified());
    }

    #[test]
    fn bare_account_has_no_domain() {
        let account = AccountIdentity::parse("alice", None);
        assert_eq!(account.local(), "alice");
        assert_eq!(account.domain(), "");
        assert_eq!(account.qualified(), "alice");
    }

    #[test]
    fn legacy_domains_are_normalized() {
        for legacy in ["szu", "@szu"] {
            let account = AccountIdentity::parse("alice", Some(legacy));
            assert_eq!(account.local(), "alice");
            assert_eq!(account.domain(), "@szu");
            assert_eq!(account.qualified(), "alice@szu");
        }
    }

    #[test]
    fn embedded_domain_wins_over_legacy_domain() {
        let account = AccountIdentity::parse("alice@staff", Some("student"));
        assert_eq!(account.local(), "alice");
        assert_eq!(account.domain(), "@staff");
        assert_eq!(account.qualified(), "alice@staff");
    }

    #[test]
    fn trailing_at_preserves_existing_protocol_concatenation() {
        let without_legacy = AccountIdentity::parse("alice@", None);
        assert_eq!(without_legacy.local(), "alice@");
        assert_eq!(without_legacy.domain(), "");
        assert_eq!(without_legacy.qualified(), "alice@");

        let with_legacy = AccountIdentity::parse("alice@", Some("szu"));
        assert_eq!(with_legacy.local(), "alice@");
        assert_eq!(with_legacy.domain(), "@szu");
        assert_eq!(with_legacy.qualified(), "alice@@szu");
    }
}
