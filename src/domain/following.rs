//! Domain values for the virtual personal activity view.
//!
//! Following updates describe why a mail thread is relevant to the current
//! user. They are kept separate from IMAP mailbox names because Following is a
//! derived view, not a remote mailbox.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum FollowingKind {
    ToMe,
    CcMe,
    SentPatch,
    SentReply,
}

impl FollowingKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ToMe => "to_me",
            Self::CcMe => "cc_me",
            Self::SentPatch => "sent_patch",
            Self::SentReply => "sent_reply",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::ToMe => "TO",
            Self::CcMe => "CC",
            Self::SentPatch => "SENT",
            Self::SentReply => "SENT",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "to_me" => Some(Self::ToMe),
            "cc_me" => Some(Self::CcMe),
            "sent_patch" => Some(Self::SentPatch),
            "sent_reply" => Some(Self::SentReply),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FollowingKind;

    #[test]
    fn following_kind_roundtrips_storage_labels() {
        for kind in [
            FollowingKind::ToMe,
            FollowingKind::CcMe,
            FollowingKind::SentPatch,
            FollowingKind::SentReply,
        ] {
            assert_eq!(FollowingKind::from_str(kind.as_str()), Some(kind));
        }
    }
}
