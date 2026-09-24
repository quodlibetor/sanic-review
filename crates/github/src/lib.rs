//! GitHub REST and GraphQL client.
//!
//! Everything here reads, except the write paths in `review` (creating,
//! replying in, submitting and deleting a review, and reacting), which only
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
pub use review::{
    NewComment, NewReaction, NewReply, NewReview, PENDING_REVIEW, PendingReview, ReviewEvent,
    ReviewStatus, Step, review_state_step,
};
pub use token::Token;
