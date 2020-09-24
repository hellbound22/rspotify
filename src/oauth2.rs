//! User authorization and client credentials management.

use chrono::prelude::*;
use derive_builder::Builder;
use maybe_async::maybe_async;
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use serde::{Deserialize, Serialize};
use serde_json::json;

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::iter::FromIterator;

use super::client::{ClientResult, Spotify};
use super::http::{headers, BaseClient, FormData, Headers};
use super::json_insert;
use super::util::{convert_map_to_string, datetime_to_timestamp, generate_random_string};

const PATH_SEGMENT_ENCODE_SET: &AsciiSet = &CONTROLS.add(b'%').add(b'/');

mod auth_urls {
    pub const AUTHORIZE: &str = "https://accounts.spotify.com/authorize";
    pub const TOKEN: &str = "https://accounts.spotify.com/api/token";
}

/// Spotify access token information.
///
/// TODO: does this need the builder pattern?
#[derive(Builder, Clone, Debug, Serialize, Deserialize)]
pub struct Token {
    #[builder(setter(into))]
    pub access_token: String,
    pub expires_in: u32,
    #[builder(setter(strip_option), default)]
    pub expires_at: Option<i64>,
    #[builder(setter(into, strip_option), default)]
    pub refresh_token: Option<String>,
    #[builder(setter(into))]
    pub scope: String,
}

/// Simple client credentials object for Spotify.
#[derive(Builder, Debug, Default, Clone, Serialize, Deserialize)]
pub struct Credentials {
    #[builder(setter(into))]
    pub id: String,
    #[builder(setter(into))]
    pub secret: String,
}

/// Structure that holds the required information for requests with OAuth.
#[derive(Builder, Debug, Default, Clone, Serialize, Deserialize)]
pub struct OAuth {
    #[builder(setter(into))]
    pub redirect_uri: String,
    /// The state is generated by default, as suggested by the OAuth2 spec:
    /// https://tools.ietf.org/html/rfc6749#section-10.12
    #[builder(setter(into), default = "generate_random_string(16)")]
    pub state: String,
    #[builder(setter(into))]
    pub scope: String,
    #[builder(setter(into, strip_option), default)]
    pub proxies: Option<String>,
}

impl CredentialsBuilder {
    /// Parses the credentials from the environment variables
    /// `RSPOTIFY_CLIENT_ID` and `RSPOTIFY_CLIENT_SECRET`. You can optionally
    /// activate the `env-file` feature in order to read these variables from
    /// a `.env` file.
    pub fn from_env() -> Self {
        #[cfg(feature = "env-file")]
        {
            dotenv::dotenv().ok();
        }
        let id = env::var("RSPOTIFY_CLIENT_ID").ok();
        let secret = env::var("RSPOTIFY_CLIENT_SECRET").ok();

        CredentialsBuilder {
            id,
            secret,
            ..Default::default()
        }
    }
}

impl OAuthBuilder {
    /// Parses the credentials from the environment variable
    /// `RSPOTIFY_REDIRECT_URI`. You can optionally activate the `env-file`
    /// feature in order to read these variables from a `.env` file.
    pub fn from_env() -> Self {
        #[cfg(feature = "env-file")]
        {
            dotenv::dotenv().ok();
        }
        let redirect_uri = env::var("RSPOTIFY_REDIRECT_URI").ok();

        OAuthBuilder {
            redirect_uri,
            ..Default::default()
        }
    }
}

/// Authorization-related methods for the client.
impl Spotify {
    /// TODO this should be removed after making a custom type for scopes
    /// or handling them as a vector of strings.
    fn is_scope_subset(needle_scope: &mut str, haystack_scope: &mut str) -> bool {
        let needle_vec: Vec<&str> = needle_scope.split_whitespace().collect();
        let haystack_vec: Vec<&str> = haystack_scope.split_whitespace().collect();
        let needle_set: HashSet<&str> = HashSet::from_iter(needle_vec);
        let haystack_set: HashSet<&str> = HashSet::from_iter(haystack_vec);
        // needle_set - haystack_set
        needle_set.is_subset(&haystack_set)
    }

    /// TODO: we should use `Instant` for expiration dates, which requires this
    /// to be modified.
    /// TODO: this should be moved into `Token`
    fn is_token_expired(&self, token_info: &Token) -> bool {
        let now: DateTime<Utc> = Utc::now();

        // 10s as buffer time
        match token_info.expires_at {
            Some(expires_at) => now.timestamp() > expires_at - 10,
            None => true,
        }
    }

    /// Saves the internal access token information into its cache file.
    fn save_token_info(&self) -> ClientResult<()> {
        let token_info = serde_json::to_string(&self.token)?;

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .open(self.cache_path.as_path())?;
        file.set_len(0)?;
        file.write_all(token_info.as_bytes())?;

        Ok(())
    }

    /// Gets the required URL to authorize the current client to start the
    /// [Authorization Code Flow](https://developer.spotify.com/documentation/general/guides/authorization-guide/#authorization-code-flow).
    pub fn get_authorize_request_url(&self, show_dialog: bool) -> ClientResult<String> {
        let oauth = self.get_oauth()?;
        let mut payload = json! ({
            "client_id": &self.get_creds()?.id,
            "response_type": "code",
            // TODO: Maybe these OAuth options should go in a struct, or
            // `Credentials` could be expanded with these.
            "redirect_uri": &oauth.redirect_uri,
            "scope": &oauth.scope,
            "state": &oauth.state
        });

        if show_dialog {
            json_insert!(payload, "show_dialog", "true");
        }

        // FIXME for some reason to_string for Value::String::to_string adds
        // quotes if as_str isn't used. This is horrendous and temporary.
        //
        // TODO: Perhaps the `BaseClient` implementation should provide this
        // method, so that reqwest can use its own implementation.
        let query_str = convert_map_to_string(
            payload
                .as_object()
                .unwrap()
                .into_iter()
                .map(|(key, val)| (key.to_owned(), val.as_str().unwrap().to_owned()))
                .collect::<HashMap<String, String>>(),
        );
        let encoded = &utf8_percent_encode(&query_str, PATH_SEGMENT_ENCODE_SET);
        let url = format!("{}?{}", auth_urls::AUTHORIZE, encoded);

        Ok(url)
    }

    /// Tries to read the cache file's token, which may not exist.
    #[maybe_async]
    pub async fn get_cached_token(&mut self) -> Option<Token> {
        let mut file = fs::File::open(&self.cache_path).ok()?;
        let mut tok_str = String::new();
        file.read_to_string(&mut tok_str).ok()?;

        let mut tok: Token = serde_json::from_str(&tok_str).ok()?;

        if !Self::is_scope_subset(&mut self.get_oauth_mut().ok()?.scope, &mut tok.scope)
            || self.is_token_expired(&tok)
        {
            // Invalid token, since it doesn't have at least the currently
            // required scopes or it's expired.
            None
        } else {
            Some(tok)
        }
    }

    /// Sends a request to Spotify for an access token.
    #[maybe_async]
    async fn fetch_access_token(&self, payload: &FormData) -> ClientResult<Token> {
        // This request uses a specific content type, and the client ID/secret
        // as the authentication, since the access token isn't available yet.
        let mut head = Headers::new();
        let (key, val) = headers::basic_auth(&self.get_creds()?.id, &self.get_creds()?.secret);
        head.insert(key, val);

        let response = self
            .post_form(auth_urls::TOKEN, Some(&head), payload)
            .await?;
        let mut tok = serde_json::from_str::<Token>(&response)?;
        tok.expires_at = Some(datetime_to_timestamp(tok.expires_in));

        Ok(tok)
    }

    /// Refreshes the access token with the refresh token provided by the
    /// [Authorization Code Flow](https://developer.spotify.com/documentation/general/guides/authorization-guide/#authorization-code-flow),
    /// without saving it into the cache file.
    ///
    /// The obtained token will be saved internally.
    #[maybe_async]
    pub async fn refresh_user_token_without_cache(
        &mut self,
        refresh_token: &str,
    ) -> ClientResult<()> {
        let mut data = FormData::new();
        data.insert("refresh_token".to_owned(), refresh_token.to_owned());
        data.insert("grant_type".to_owned(), "refresh_token".to_owned());

        let mut tok = self.fetch_access_token(&data).await?;
        tok.refresh_token = Some(refresh_token.to_string());
        self.token = Some(tok);

        Ok(())
    }

    /// The same as `refresh_user_token_without_cache`, but saves the token
    /// into the cache file if possible.
    #[maybe_async]
    pub async fn refresh_user_token(&mut self, refresh_token: &str) -> ClientResult<()> {
        self.refresh_user_token_without_cache(refresh_token).await?;
        self.save_token_info()
    }

    /// Obtains the client access token for the app without saving it into the
    /// cache file. The resulting token is saved internally.
    #[maybe_async]
    pub async fn request_client_token_without_cache(&mut self) -> ClientResult<()> {
        let mut data = FormData::new();
        data.insert("grant_type".to_owned(), "client_credentials".to_owned());

        self.token = Some(self.fetch_access_token(&data).await?);

        Ok(())
    }

    /// The same as `request_client_token_without_cache`, but saves the token
    /// into the cache file if possible.
    #[maybe_async]
    pub async fn request_client_token(&mut self) -> ClientResult<()> {
        self.request_client_token_without_cache().await?;
        self.save_token_info()
    }

    /// Obtains the user access token for the app with the given code without
    /// saving it into the cache file, as part of the OAuth authentication.
    /// The access token will be saved inside the Spotify instance.
    ///
    /// Step 3 of the [Authorization Code Flow](https://developer.spotify.com/documentation/general/guides/authorization-guide/#authorization-code-flow).
    #[maybe_async]
    pub async fn request_user_token_without_cache(&mut self, code: &str) -> ClientResult<()> {
        let oauth = self.get_oauth()?;
        let mut data = FormData::new();
        data.insert("grant_type".to_owned(), "authorization_code".to_owned());
        data.insert("redirect_uri".to_owned(), oauth.redirect_uri.clone());
        data.insert("code".to_owned(), code.to_owned());
        data.insert("scope".to_owned(), oauth.scope.clone());
        data.insert("state".to_owned(), oauth.state.clone());

        self.token = Some(self.fetch_access_token(&data).await?);

        Ok(())
    }

    /// The same as `request_user_token_without_cache`, but saves the token into
    /// the cache file if possible.
    #[maybe_async]
    pub async fn request_user_token(&mut self, code: &str) -> ClientResult<()> {
        self.request_user_token_without_cache(code).await?;
        self.save_token_info()
    }

    /// Opens up the authorization URL in the user's browser so that it can
    /// authenticate. It also reads from the standard input the redirect URI
    /// in order to obtain the access token information. The resulting access
    /// token will be saved internally once the operation is successful.
    #[cfg(feature = "cli")]
    #[maybe_async]
    pub async fn prompt_for_user_token_without_cache(&mut self) -> ClientResult<()> {
        let code = self.get_code_from_user()?;
        self.request_user_token_without_cache(&code).await?;

        Ok(())
    }

    /// The same as the `prompt_for_user_token_without_cache` method, but it
    /// will try to use the user token into the cache file, and save it in
    /// case it didn't exist/was invalid.
    #[cfg(feature = "cli")]
    #[maybe_async]
    pub async fn prompt_for_user_token(&mut self) -> ClientResult<()> {
        // TODO: not sure where the cached token should be read. Should it
        // be more explicit? Also outside of this function?
        // TODO: shouldn't this also refresh the obtained token?
        self.token = self.get_cached_token().await;

        // Otherwise following the usual procedure to get the token.
        if self.token.is_none() {
            let code = self.get_code_from_user()?;
            // Will write to the cache file if successful
            self.request_user_token(&code).await?;
        }

        Ok(())
    }

    /// Tries to open the authorization URL in the user's browser, and returns
    /// the obtained code.
    #[cfg(feature = "cli")]
    fn get_code_from_user(&self) -> ClientResult<String> {
        use crate::client::ClientError;

        let url = self.get_authorize_request_url(false)?;

        match webbrowser::open(&url) {
            Ok(_) => println!("Opened {} in your browser.", url),
            Err(why) => eprintln!(
                "Error when trying to open an URL in your browser: {:?}. \
                 Please navigate here manually: {}",
                why, url
            ),
        }

        println!("Please enter the URL you were redirected to: ");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let code = self
            .parse_response_code(&input)
            .ok_or_else(|| ClientError::CLI("unable to parse the response code".to_string()))?;

        Ok(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::SpotifyBuilder;

    use std::fs;
    use std::io::Read;

    #[test]
    fn test_is_scope_subset() {
        let mut needle_scope = String::from("1 2 3");
        let mut haystack_scope = String::from("1 2 3 4");
        let mut broken_scope = String::from("5 2 4");
        assert!(Spotify::is_scope_subset(
            &mut needle_scope,
            &mut haystack_scope
        ));
        assert!(!Spotify::is_scope_subset(
            &mut broken_scope,
            &mut haystack_scope
        ));
    }

    #[test]
    fn test_save_token_info() {
        let tok = TokenBuilder::default()
            .access_token("test-access_token")
            .expires_in(3600)
            .expires_at(1515841743)
            .scope("playlist-read-private playlist-read-collaborative playlist-modify-public playlist-modify-private streaming ugc-image-upload user-follow-modify user-follow-read user-library-read user-library-modify user-read-private user-read-birthdate user-read-email user-top-read user-read-playback-state user-modify-playback-state user-read-currently-playing user-read-recently-played")
            .refresh_token("...")
            .build()
            .unwrap();

        let spotify = SpotifyBuilder::default()
            .token(tok.clone())
            .build()
            .unwrap();

        let tok_str = serde_json::to_string(&tok).unwrap();

        spotify.save_token_info().unwrap();

        let mut file = fs::File::open(&spotify.cache_path).unwrap();
        let mut tok_str_file = String::new();
        file.read_to_string(&mut tok_str_file).unwrap();

        assert_eq!(tok_str, tok_str_file);
    }
}
