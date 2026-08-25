//! identity — Known users, user registry, and user profiles (L2 service layer).

pub mod known_users;
pub mod user_profile;
pub mod user_registry;

pub use known_users::{
    ContactDirection, ContactEntry, ContactStatus, DeliveryVerdict, KnownUser,
    KnownUsersRegistry, RequestOutcome, UserMail,
};
pub use user_profile::{UserProfile, UserResolver};
pub use user_registry::{
    RegisterError, User, UserRegistry, DEFAULT_NAMESPACE, validate_email, validate_username,
};
