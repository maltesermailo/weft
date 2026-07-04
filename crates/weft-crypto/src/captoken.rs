//! Scoped capability tokens (spec §10.4):
//!
//! ```text
//! token = sign(issuer_key, {
//!   subject: <pubkey|account> | UNBOUND,
//!   scope:   <#chan> | ns:<name> | *,
//!   caps:    [...],
//!   epoch:   <scope revocation epoch at issue>,
//!   expiry:  <unix seconds>,
//!   parent:  <hash of parent token> | none
//! })
//! ```
//!
//! Deterministic CBOR encode-before-sign (CLAUDE.md): the signed body is a
//! positional array, so bytes are canonical with no map-ordering to fuzz.
//! Delegation is verified as a chain from a root signed by the scope
//! authority (namespace root key or network key) down to the leaf; each
//! step must narrow scope and hold `grant:<cap>` for every capability it
//! passes on. No clock and no store here — `now` and the epoch lookup are
//! parameters, so the whole thing is pure and fuzzable.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::caps::Capability;
use crate::keys::{Keypair, PublicKey, Signature};
use crate::{b64, CryptoError};

const VERSION: u8 = 1;

/// Who a token authorizes. Only `Key` subjects can sign child tokens
/// (delegate further); `Account` subjects are leaves used by that account;
/// `Unbound` is an invite (§6.5) bound to a redeemer's key on redemption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    Key(PublicKey),
    Account(String),
    Unbound,
}

impl Subject {
    fn as_key(&self) -> Option<&PublicKey> {
        match self {
            Subject::Key(key) => Some(key),
            _ => None,
        }
    }

    // (tag, payload) — deterministic 2-field encoding.
    fn encode(&self) -> (u8, Vec<u8>) {
        match self {
            Subject::Key(key) => (0, key.as_bytes().to_vec()),
            Subject::Account(name) => (1, name.as_bytes().to_vec()),
            Subject::Unbound => (2, Vec::new()),
        }
    }

    fn decode(tag: u8, payload: Vec<u8>) -> Result<Self, CryptoError> {
        match tag {
            0 => Ok(Subject::Key(PublicKey::from_bytes(&payload)?)),
            1 => Ok(Subject::Account(
                String::from_utf8(payload).map_err(|_| CryptoError::BadToken)?,
            )),
            2 => Ok(Subject::Unbound),
            _ => Err(CryptoError::BadToken),
        }
    }
}

/// A token's scope (§10.4). Kept as parsed variants so `covers` can express
/// that a namespace scope covers the channels inside it — the only piece of
/// protocol grammar this crate needs (a channel `#ns/chan` names its
/// namespace in its first segment, §2.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenScope {
    /// `*` — the network signing key's scope.
    Wildcard,
    /// `ns:<name>`.
    Namespace(String),
    /// `#chan` or `#ns/chan`.
    Channel(String),
}

impl TokenScope {
    pub fn parse(s: &str) -> Option<Self> {
        if s == "*" {
            Some(TokenScope::Wildcard)
        } else if let Some(name) = s.strip_prefix("ns:") {
            (!name.is_empty()).then(|| TokenScope::Namespace(name.to_string()))
        } else if s.starts_with('#') {
            Some(TokenScope::Channel(s.to_string()))
        } else {
            None
        }
    }

    pub fn as_str(&self) -> String {
        match self {
            TokenScope::Wildcard => "*".to_string(),
            TokenScope::Namespace(name) => format!("ns:{name}"),
            TokenScope::Channel(chan) => chan.clone(),
        }
    }

    /// The namespace segment of a channel scope, if any (`#foo/bar` → foo).
    fn channel_namespace(chan: &str) -> Option<&str> {
        chan.strip_prefix('#')?.split_once('/').map(|(ns, _)| ns)
    }

    /// Does authority at `self` cover an object at `other`? Wildcard covers
    /// all; a namespace covers itself and its channels; a channel covers
    /// only itself.
    pub fn covers(&self, other: &TokenScope) -> bool {
        match (self, other) {
            (TokenScope::Wildcard, _) => true,
            (TokenScope::Namespace(a), TokenScope::Namespace(b)) => a == b,
            (TokenScope::Namespace(a), TokenScope::Channel(c)) => {
                Self::channel_namespace(c) == Some(a.as_str())
            }
            (TokenScope::Channel(a), TokenScope::Channel(b)) => a == b,
            _ => false,
        }
    }
}

/// The signed body. `issuer` is in the body so a token is self-describing:
/// verification checks the signature against it AND that it equals the
/// expected signer (authority for the root, parent's subject for a child).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub issuer: PublicKey,
    pub subject: Subject,
    pub scope: TokenScope,
    pub caps: Vec<Capability>,
    pub epoch: u64,
    pub expiry: u64,
    /// Hash of the parent token; `None` at the root.
    pub parent: Option<[u8; 32]>,
}

/// CBOR positional form: version, issuer, subject-tag, subject-payload,
/// scope, caps, epoch, expiry, parent.
#[derive(Serialize, Deserialize)]
struct Wire(
    u8,
    Vec<u8>,
    u8,
    Vec<u8>,
    String,
    Vec<String>,
    u64,
    u64,
    Option<Vec<u8>>,
);

impl Grant {
    fn to_wire(&self) -> Wire {
        let (tag, payload) = self.subject.encode();
        Wire(
            VERSION,
            self.issuer.as_bytes().to_vec(),
            tag,
            payload,
            self.scope.as_str(),
            self.caps.iter().map(Capability::to_string).collect(),
            self.epoch,
            self.expiry,
            self.parent.map(|h| h.to_vec()),
        )
    }

    /// The exact bytes signed and hashed.
    fn signing_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        ciborium::into_writer(&self.to_wire(), &mut bytes).expect("CBOR to Vec cannot fail");
        bytes
    }

    /// Sign with the issuer's keypair (issuer field must be its public key).
    pub fn sign(self, issuer: &Keypair) -> Token {
        debug_assert_eq!(
            self.issuer,
            issuer.public(),
            "issuer field must match signer"
        );
        let signature = issuer.sign(&self.signing_bytes());
        Token {
            grant: self,
            signature,
        }
    }

    fn from_wire(wire: Wire) -> Result<Self, CryptoError> {
        let Wire(version, issuer, tag, payload, scope, caps, epoch, expiry, parent) = wire;
        if version != VERSION {
            return Err(CryptoError::BadToken);
        }
        let parent = match parent {
            None => None,
            Some(bytes) => Some(bytes.try_into().map_err(|_| CryptoError::BadToken)?),
        };
        Ok(Grant {
            issuer: PublicKey::from_bytes(&issuer)?,
            subject: Subject::decode(tag, payload)?,
            scope: TokenScope::parse(&scope).ok_or(CryptoError::BadToken)?,
            caps: caps.iter().map(|c| c.parse()).collect::<Result<_, _>>()?,
            epoch,
            expiry,
            parent,
        })
    }
}

/// A signed capability token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub grant: Grant,
    pub signature: Signature,
}

impl Token {
    /// Content hash for parent linkage (of the signed body bytes).
    pub fn hash(&self) -> [u8; 32] {
        Sha256::digest(self.grant.signing_bytes()).into()
    }

    pub fn to_b64(&self) -> String {
        let mut bytes = Vec::new();
        let wire = (self.grant.to_wire(), self.signature.to_bytes().to_vec());
        ciborium::into_writer(&wire, &mut bytes).expect("CBOR to Vec cannot fail");
        b64::encode(bytes)
    }

    pub fn from_b64(s: &str) -> Result<Self, CryptoError> {
        let bytes = b64::decode(s)?;
        let (wire, sig): (Wire, Vec<u8>) =
            ciborium::from_reader(bytes.as_slice()).map_err(|_| CryptoError::BadToken)?;
        Ok(Token {
            grant: Grant::from_wire(wire)?,
            signature: Signature::from_slice(&sig).map_err(|_| CryptoError::BadToken)?,
        })
    }
}

/// What a verified chain proves: the leaf's subject may exercise `caps`
/// within `scope`. The enforcement layer (weft-core) checks this against
/// the specific action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub subject: Subject,
    pub scope: TokenScope,
    pub caps: Vec<Capability>,
}

impl Verified {
    /// Does the chain authorize `cap` for an object at `scope`?
    pub fn authorizes(&self, cap: &Capability, scope: &TokenScope) -> bool {
        self.scope.covers(scope) && self.caps.contains(cap)
    }
}

/// True if a holder of `parent_caps` may delegate `cap` — i.e. it holds
/// `grant:<cap>`. Nesting composes: delegating `grant:send` needs
/// `grant:grant:send`.
fn can_delegate(parent_caps: &[Capability], cap: &Capability) -> bool {
    parent_caps.contains(&Capability::Grant(Box::new(cap.clone())))
}

/// Verify a root→leaf delegation chain against the scope `authority` key
/// (namespace root or network key) at time `now`, with `epoch_of` giving
/// the current revocation epoch per scope. Returns what the leaf proves.
///
/// Every structural rule is enforced (invariant 4 depends on it): root
/// signed by the authority and unrooted, each link signed by its parent's
/// subject key, `parent` hash matches, scope narrows, caps are delegable,
/// nothing expired, nothing revoked.
pub fn verify_chain(
    chain: &[Token],
    authority: &PublicKey,
    now: u64,
    epoch_of: impl Fn(&TokenScope) -> u64,
) -> Result<Verified, CryptoError> {
    let root = chain.first().ok_or(CryptoError::BadToken)?;
    if root.grant.parent.is_some() || &root.grant.issuer != authority {
        return Err(CryptoError::Unauthorized);
    }

    for (i, token) in chain.iter().enumerate() {
        let g = &token.grant;
        // Signature must verify against the issuer named in the body.
        g.issuer.verify(&g.signing_bytes(), &token.signature)?;
        if now >= g.expiry {
            return Err(CryptoError::Expired);
        }
        if g.epoch < epoch_of(&g.scope) {
            return Err(CryptoError::Revoked);
        }
        if i == 0 {
            continue; // root: issuer==authority already checked
        }
        let parent = &chain[i - 1];
        // The parent's subject must be a key (only keys can sign children),
        // and must be this token's issuer.
        let parent_key = parent
            .grant
            .subject
            .as_key()
            .ok_or(CryptoError::Unauthorized)?;
        if &g.issuer != parent_key {
            return Err(CryptoError::Unauthorized);
        }
        if g.parent != Some(parent.hash()) {
            return Err(CryptoError::Unauthorized);
        }
        if !parent.grant.scope.covers(&g.scope) {
            return Err(CryptoError::Unauthorized);
        }
        for cap in &g.caps {
            if !can_delegate(&parent.grant.caps, cap) {
                return Err(CryptoError::Unauthorized);
            }
        }
    }

    let leaf = chain.last().expect("non-empty checked above");
    Ok(Verified {
        subject: leaf.grant.subject.clone(),
        scope: leaf.grant.scope.clone(),
        caps: leaf.grant.caps.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEVER: u64 = u64::MAX;

    fn no_revocations(_: &TokenScope) -> u64 {
        0
    }

    fn cap(s: &str) -> Capability {
        s.parse().unwrap()
    }

    /// A root token signed by `authority` granting `caps` at `scope` to `subject`.
    fn root(
        authority: &Keypair,
        subject: Subject,
        scope: &str,
        caps: &[&str],
        epoch: u64,
    ) -> Token {
        Grant {
            issuer: authority.public(),
            subject,
            scope: TokenScope::parse(scope).unwrap(),
            caps: caps.iter().map(|c| cap(c)).collect(),
            epoch,
            expiry: NEVER,
            parent: None,
        }
        .sign(authority)
    }

    fn child(
        issuer: &Keypair,
        parent: &Token,
        subject: Subject,
        scope: &str,
        caps: &[&str],
    ) -> Token {
        Grant {
            issuer: issuer.public(),
            subject,
            scope: TokenScope::parse(scope).unwrap(),
            caps: caps.iter().map(|c| cap(c)).collect(),
            epoch: 0,
            expiry: NEVER,
            parent: Some(parent.hash()),
        }
        .sign(issuer)
    }

    #[test]
    fn single_root_token_authorizes_its_caps() {
        let net = Keypair::generate();
        let ada = Keypair::generate();
        let token = root(
            &net,
            Subject::Key(ada.public()),
            "#general",
            &["send", "react"],
            0,
        );
        let verified = verify_chain(&[token], &net.public(), 1_000, no_revocations).unwrap();
        assert!(verified.authorizes(&cap("send"), &TokenScope::parse("#general").unwrap()));
        assert!(!verified.authorizes(&cap("ban"), &TokenScope::parse("#general").unwrap()));
        // Scope doesn't cover a different channel.
        assert!(!verified.authorizes(&cap("send"), &TokenScope::parse("#other").unwrap()));
    }

    #[test]
    fn wildcard_root_covers_namespaces_and_channels() {
        let net = Keypair::generate();
        let op = Keypair::generate();
        let token = root(
            &net,
            Subject::Key(op.public()),
            "*",
            &["netblock", "ns-create"],
            0,
        );
        let v = verify_chain(&[token], &net.public(), 0, no_revocations).unwrap();
        assert!(v.authorizes(&cap("netblock"), &TokenScope::parse("#any/chan").unwrap()));
        assert!(v.authorizes(&cap("ns-create"), &TokenScope::parse("ns:foo").unwrap()));
    }

    #[test]
    fn delegation_chain_narrows_scope_and_caps() {
        let net = Keypair::generate();
        let admin = Keypair::generate();
        let mod_ = Keypair::generate();
        // Namespace root can grant everything, incl. the right to delegate.
        let r = root(
            &net,
            Subject::Key(admin.public()),
            "ns:gaming",
            &["ban", "grant:ban", "grant:send"],
            0,
        );
        // Admin delegates `ban` (only) down to a channel.
        let c = child(
            &admin,
            &r,
            Subject::Key(mod_.public()),
            "#gaming/general",
            &["ban"],
        );
        let v = verify_chain(&[r, c], &net.public(), 0, no_revocations).unwrap();
        assert!(v.authorizes(&cap("ban"), &TokenScope::parse("#gaming/general").unwrap()));
        // The delegate got ban but not send, and only in that channel.
        assert!(!v.authorizes(&cap("send"), &TokenScope::parse("#gaming/general").unwrap()));
        assert!(!v.authorizes(&cap("ban"), &TokenScope::parse("#gaming/other").unwrap()));
    }

    #[test]
    fn cannot_delegate_without_the_grant_right() {
        let net = Keypair::generate();
        let admin = Keypair::generate();
        let mod_ = Keypair::generate();
        // Admin holds `ban` but NOT `grant:ban` — cannot pass it on.
        let r = root(&net, Subject::Key(admin.public()), "ns:x", &["ban"], 0);
        let c = child(&admin, &r, Subject::Key(mod_.public()), "ns:x", &["ban"]);
        assert_eq!(
            verify_chain(&[r, c], &net.public(), 0, no_revocations),
            Err(CryptoError::Unauthorized)
        );
    }

    #[test]
    fn cannot_widen_scope_when_delegating() {
        let net = Keypair::generate();
        let admin = Keypair::generate();
        let sub = Keypair::generate();
        let r = root(
            &net,
            Subject::Key(admin.public()),
            "#gaming/general",
            &["grant:send"],
            0,
        );
        // Try to hand out `send` at the whole namespace — wider than parent.
        let c = child(
            &admin,
            &r,
            Subject::Key(sub.public()),
            "ns:gaming",
            &["send"],
        );
        assert_eq!(
            verify_chain(&[r, c], &net.public(), 0, no_revocations),
            Err(CryptoError::Unauthorized)
        );
    }

    #[test]
    fn forged_links_are_rejected() {
        let net = Keypair::generate();
        let admin = Keypair::generate();
        let attacker = Keypair::generate();
        let r = root(
            &net,
            Subject::Key(admin.public()),
            "ns:x",
            &["grant:ban"],
            0,
        );
        // Attacker forges a child not actually signed by admin's subject key.
        let forged = child(
            &attacker,
            &r,
            Subject::Key(attacker.public()),
            "ns:x",
            &["ban"],
        );
        assert_eq!(
            verify_chain(&[r, forged], &net.public(), 0, no_revocations),
            Err(CryptoError::Unauthorized)
        );

        // Root not signed by the claimed authority.
        let wrong_auth = Keypair::generate();
        let r2 = root(&net, Subject::Key(admin.public()), "ns:x", &["ban"], 0);
        assert_eq!(
            verify_chain(&[r2], &wrong_auth.public(), 0, no_revocations),
            Err(CryptoError::Unauthorized)
        );
    }

    #[test]
    fn tampered_body_breaks_the_signature() {
        let net = Keypair::generate();
        let ada = Keypair::generate();
        let mut token = root(&net, Subject::Key(ada.public()), "#general", &["send"], 0);
        token.grant.caps.push(cap("ban")); // escalate after signing
        assert_eq!(
            verify_chain(&[token], &net.public(), 0, no_revocations),
            Err(CryptoError::BadSignature)
        );
    }

    #[test]
    fn expiry_and_revocation_epoch_enforced() {
        let net = Keypair::generate();
        let ada = Keypair::generate();
        let mut g = Grant {
            issuer: net.public(),
            subject: Subject::Key(ada.public()),
            scope: TokenScope::parse("#general").unwrap(),
            caps: vec![cap("send")],
            epoch: 5,
            expiry: 1_000,
            parent: None,
        };
        let token = g.clone().sign(&net);
        assert!(verify_chain(
            std::slice::from_ref(&token),
            &net.public(),
            999,
            no_revocations
        )
        .is_ok());
        // Expired.
        assert_eq!(
            verify_chain(
                std::slice::from_ref(&token),
                &net.public(),
                1_000,
                no_revocations
            ),
            Err(CryptoError::Expired)
        );
        // Scope epoch bumped past the token's issue epoch → revoked.
        assert_eq!(
            verify_chain(&[token], &net.public(), 0, |_| 6),
            Err(CryptoError::Revoked)
        );
        // A token minted at the new epoch survives.
        g.epoch = 6;
        let fresh = g.sign(&net);
        assert!(verify_chain(&[fresh], &net.public(), 0, |_| 6).is_ok());
    }

    #[test]
    fn revoking_a_parent_scope_kills_the_chain() {
        let net = Keypair::generate();
        let admin = Keypair::generate();
        let mod_ = Keypair::generate();
        let r = root(
            &net,
            Subject::Key(admin.public()),
            "ns:x",
            &["grant:ban"],
            3,
        );
        let c = child(
            &admin,
            &r,
            Subject::Key(mod_.public()),
            "#x/general",
            &["ban"],
        );
        // Bumping the ns:x epoch invalidates the parent link → whole chain.
        let epoch_of = |scope: &TokenScope| match scope {
            TokenScope::Namespace(n) if n == "x" => 4,
            _ => 0,
        };
        assert_eq!(
            verify_chain(&[r, c], &net.public(), 0, epoch_of),
            Err(CryptoError::Revoked)
        );
    }

    #[test]
    fn account_and_unbound_subjects_cannot_sign_children() {
        let net = Keypair::generate();
        let mod_ = Keypair::generate();
        // A token granted to an account (no key) cannot delegate further.
        let r = root(
            &net,
            Subject::Account("ada".into()),
            "ns:x",
            &["grant:ban"],
            0,
        );
        let c = child(&mod_, &r, Subject::Key(mod_.public()), "ns:x", &["ban"]);
        assert_eq!(
            verify_chain(&[r, c], &net.public(), 0, no_revocations),
            Err(CryptoError::Unauthorized)
        );
    }

    #[test]
    fn token_round_trips_through_b64() {
        let net = Keypair::generate();
        let token = root(
            &net,
            Subject::Account("ada".into()),
            "ns:gaming",
            &["ns-admin", "grant:send"],
            2,
        );
        let restored = Token::from_b64(&token.to_b64()).unwrap();
        assert_eq!(restored, token);
        assert_eq!(restored.hash(), token.hash());
        assert!(verify_chain(&[restored], &net.public(), 0, no_revocations).is_ok());

        assert!(Token::from_b64("!!!").is_err());
        assert!(Token::from_b64(&b64::encode(b"not cbor")).is_err());
    }

    #[test]
    fn unbound_invite_token_verifies_but_binds_nothing_yet() {
        // §6.5: invites are UNBOUND capability tokens; redemption (M4-4)
        // mints a member token bound to the redeemer. Here we only prove
        // the unbound token itself verifies against the authority.
        let net = Keypair::generate();
        let token = root(&net, Subject::Unbound, "ns:gaming", &["view", "send"], 0);
        let v = verify_chain(&[token], &net.public(), 0, no_revocations).unwrap();
        assert_eq!(v.subject, Subject::Unbound);
        assert!(v.authorizes(&cap("view"), &TokenScope::parse("ns:gaming").unwrap()));
    }
}
