//! GitHub REST and GraphQL client.
//!
//! Everything here is read-only for now. Write paths (posting reviews and
//! replies) will live here too, but only `sanic-web` may call them.

mod client;
mod graphql;
mod notifications;
mod token;

pub use client::{ApiError, Client};
pub use notifications::{Notification, NotificationPoll};
pub use token::Token;
