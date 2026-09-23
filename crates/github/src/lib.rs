//! GitHub REST and GraphQL client.
//!
//! Everything here reads, except [`Client::post_review`], which only
//! `sanic-web` may call.

mod client;
mod graphql;
mod notifications;
mod review;
#[cfg(test)]
mod schema_check;
mod token;

pub use client::{ApiError, Client};
pub use notifications::{Notification, NotificationPoll};
pub use review::{NewComment, NewReview, PostedReview, ReviewEvent};
pub use token::Token;
