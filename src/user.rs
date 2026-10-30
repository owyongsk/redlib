#![allow(clippy::cmp_owned)]
use crate::client::json;
use crate::server::RequestExt;
use crate::utils::{error, filter_posts, format_url, get_filters, nsfw_landing, param, setting, template, Post, Preferences, User};
use crate::{config, utils};
use askama::Template;
use chrono::DateTime;
use htmlescape::decode_html;
use hyper::{Body, Request, Response};
use serde_json::Value;
use time::{macros::format_description, OffsetDateTime};

// STRUCTS
#[derive(Template)]
#[template(path = "user.html")]
struct UserTemplate {
	user: User,
	posts: Vec<Post>,
	sort: (String, String),
	ends: (String, String),
	/// "overview", "comments", or "submitted"
	listing: String,
	prefs: Preferences,
	url: String,
	redirect_url: String,
	/// Whether the user themself is filtered.
	is_filtered: bool,
	/// Whether all fetched posts are filtered (to differentiate between no posts fetched in the first place,
	/// and all fetched posts being filtered).
	all_posts_filtered: bool,
	/// Whether all posts were hidden because they are NSFW (and user has disabled show NSFW)
	all_posts_hidden_nsfw: bool,
	no_posts: bool,
}

// FUNCTIONS
pub async fn profile(req: Request<Body>) -> Result<Response<Body>, String> {
	let listing = req.param("listing").unwrap_or_else(|| "overview".to_string());

	// Build the Reddit JSON API path
	let path = format!(
		"/user/{}/{listing}.json?{}&raw_json=1",
		req.param("name").unwrap_or_else(|| "reddit".to_string()),
		req.uri().query().unwrap_or_default(),
	);
	let url = String::from(req.uri().path_and_query().map_or("", |val| val.as_str()));
	let redirect_url = url[1..].replace('?', "%3F").replace('&', "%26");

	// Retrieve other variables from Redlib request
	let sort = param(&path, "sort").unwrap_or_default();
	let username = req.param("name").unwrap_or_default();

	// Retrieve info from user about page.
	let user = user(&username).await.unwrap_or_default();

	let req_url = req.uri().to_string();
	// Return landing page if this post if this Reddit deems this user NSFW,
	// but we have also disabled the display of NSFW content or if the instance
	// is SFW-only.
	if user.nsfw && utils::should_be_nsfw_gated(&req, &req_url) {
		return Ok(nsfw_landing(req, req_url).await.unwrap_or_default());
	}

	let filters = get_filters(&req);
	if filters.contains(&["u_", &username].concat()) {
		Ok(template(&UserTemplate {
			user,
			posts: Vec::new(),
			sort: (sort, param(&path, "t").unwrap_or_default()),
			ends: (param(&path, "after").unwrap_or_default(), String::new()),
			listing,
			prefs: Preferences::new(&req),
			url,
			redirect_url,
			is_filtered: true,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: false,
		}))
	} else {
		// Request user posts/comments from Reddit
		match Post::fetch(&path, false).await {
			Ok((mut posts, after)) => {
				let (_, all_posts_filtered) = filter_posts(&mut posts, &filters);
				let no_posts = posts.is_empty();
				let all_posts_hidden_nsfw = !no_posts && (posts.iter().all(|p| p.flags.nsfw) && setting(&req, "show_nsfw") != "on");
				Ok(template(&UserTemplate {
					user,
					posts,
					sort: (sort, param(&path, "t").unwrap_or_default()),
					ends: (param(&path, "after").unwrap_or_default(), after),
					listing,
					prefs: Preferences::new(&req),
					url,
					redirect_url,
					is_filtered: false,
					all_posts_filtered,
					all_posts_hidden_nsfw,
					no_posts,
				}))
			}
			// If there is an error show error page
			Err(msg) => error(req, &msg).await,
		}
	}
}

// USER
/// Reddit blocks /user/{name}/about.json, so the profile comes from the
/// user's profile subreddit, and karma and the join date from the account,
/// whose ID is read from the user's latest post or comment.
async fn user(name: &str) -> Result<User, String> {
	let profile = json(format!("/r/u_{name}/about.json?raw_json=1"), false).await?;
	let about = |item| profile["data"][item].as_str().unwrap_or_default().to_string();
	let account = account(name).await.unwrap_or_default();

	let created_unix = account["created_utc"].as_f64().or_else(|| profile["data"]["created_utc"].as_f64()).unwrap_or(0.0).round() as i64;
	let created = OffsetDateTime::from_unix_timestamp(created_unix).unwrap_or(OffsetDateTime::UNIX_EPOCH);
	let karma = ["link_karma", "comment_karma"].iter().map(|k| account[k].as_i64().unwrap_or(0)).sum();

	Ok(User {
		name: account["name"].as_str().unwrap_or(name).to_owned(),
		title: about("title"),
		icon: format_url(&about("icon_img")),
		karma,
		created: created.format(format_description!("[month repr:short] [day] '[year repr:last_two]")).unwrap_or_default(),
		banner: about("banner_img"),
		description: about("public_description"),
		nsfw: profile["data"]["over18"].as_bool().unwrap_or_default(),
	})
}

async fn account(name: &str) -> Result<Value, String> {
	let overview = json(format!("/user/{name}/overview.json?limit=1&raw_json=1"), false).await?;
	let id = overview["data"]["children"][0]["data"]["author_fullname"]
		.as_str()
		.filter(|id| id.strip_prefix("t2_").is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric())))
		.ok_or("No account ID")?
		.to_owned();
	let mut accounts = json(format!("/api/user_data_by_account_ids.json?ids={id}"), false).await?;
	Ok(accounts[id.as_str()].take())
}

pub async fn rss(req: Request<Body>) -> Result<Response<Body>, String> {
	if config::get_setting("REDLIB_ENABLE_RSS").is_none() {
		return Ok(error(req, "RSS is disabled on this instance.").await.unwrap_or_default());
	}
	use crate::utils::rewrite_urls;
	use hyper::header::CONTENT_TYPE;
	use rss::{ChannelBuilder, Item};

	// Get user
	let user_str = req.param("name").unwrap_or_default();

	let listing = req.param("listing").unwrap_or_else(|| "overview".to_string());

	// Get path
	let path = format!("/user/{user_str}/{listing}.json?{}&raw_json=1", req.uri().query().unwrap_or_default(),);

	// Get user
	let user_obj = user(&user_str).await.unwrap_or_default();

	// Get posts
	let (posts, _) = Post::fetch(&path, false).await?;

	// Build the RSS feed
	let channel = ChannelBuilder::default()
		.title(user_str)
		.description(user_obj.description)
		.items(
			posts
				.into_iter()
				.map(|post| Item {
					title: Some(post.title.to_string()),
					link: Some(format_url(&utils::get_post_url(&post))),
					author: Some(post.author.name),
					pub_date: Some(DateTime::from_timestamp(post.created_ts as i64, 0).unwrap_or_default().to_rfc2822()),
					content: Some(rewrite_urls(&decode_html(&post.body).unwrap_or_else(|_| post.body.clone()))),
					..Default::default()
				})
				.collect::<Vec<_>>(),
		)
		.build();

	// Serialize the feed to RSS
	let body = channel.to_string().into_bytes();

	// Create the HTTP response
	let mut res = Response::new(Body::from(body));
	res.headers_mut().insert(CONTENT_TYPE, hyper::header::HeaderValue::from_static("application/rss+xml"));

	Ok(res)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test(flavor = "multi_thread")]
	async fn test_fetching_user() {
		let user = user("spez").await;
		assert!(user.is_ok());
		assert!(user.unwrap().karma > 100);
	}
}
