// Reddit blocks some requests from datacenter IPs. A relay on a residential
// connection can make them instead: requests go to
// `{REDLIB_RELAY_URL}/{reddit host}{path}` with `REDLIB_RELAY_SECRET` in the
// X-Relay-Secret header.
//
// Read straight from the environment rather than `Config`, which /info.json publishes.

use std::sync::LazyLock;
use wreq::RequestBuilder;

struct Relay {
	url: String,
	secret: String,
}

static RELAY: LazyLock<Option<Relay>> = LazyLock::new(|| {
	let setting = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
	Some(Relay {
		url: setting("REDLIB_RELAY_URL")?.trim_end_matches('/').to_string(),
		secret: setting("REDLIB_RELAY_SECRET").unwrap_or_default(),
	})
});

pub fn enabled() -> bool {
	RELAY.is_some()
}

pub fn url(url: &str) -> String {
	match RELAY.as_ref() {
		Some(relay) => format!("{}/{}", relay.url, url.trim_start_matches("https://")),
		None => url.to_string(),
	}
}

pub fn sign(builder: RequestBuilder) -> RequestBuilder {
	match RELAY.as_ref() {
		Some(relay) => builder.header("X-Relay-Secret", &relay.secret),
		None => builder,
	}
}
