pub mod cache;
pub mod chat;
pub mod cli;
pub mod core;
pub mod crypto;
pub mod engine;
pub mod imgview;
pub mod llm;
pub mod logging;
pub mod protocol;
pub mod providers;
pub mod tui;

pub use cache::{
    CachedMessage, CachedSearchHit, DeliveryStatus, MailCache, PeerRecord, PeerStore,
    StoredChatLine, StoredMedia, StoredSession, SyncCursor, SyncState,
};
pub use chat::{
    ChatClient, ChatSendResult, ChatSession, ChatSessionSummary, ChatSyncResult, ContactInfo,
    NewMessageNotice,
};
pub use core::{
    MailConfig, MailCore, MailboxInfo, MailboxSession, MessageSummary, MutationRequest,
    MutationResult,
};
pub use crypto::{Identity, grouped_fingerprint};
pub use engine::{Command, Engine, Event};
pub use llm::{LlmAnalysis, LlmConfig};
pub use protocol::{IncomingSlip, MediaKind, OutgoingSlip, SlipEnvelope};
pub use providers::{Endpoint, Provider, Security};
