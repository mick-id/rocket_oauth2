use std::collections::HashMap;
use std::fmt::Debug;

use rocket::fairing::{AdHoc, Fairing};
use rocket::handler;
use rocket::http::uri::Absolute;
use rocket::http::{Cookie, Cookies, Method, SameSite, Status};
use rocket::outcome::{IntoOutcome, Outcome};
use rocket::request::{FormItems, FromForm, Request};
use rocket::response::{Redirect, Responder};
use rocket::{Data, Route, State};
use serde_json::Value as JsonValue;

use crate::OAuthConfig;

const STATE_COOKIE_NAME: &str = "rocket_oauth2_state";

/// The token types which can be exchanged with the token endpoint
#[derive(Clone, PartialEq, Debug)]
pub enum TokenRequest {
    /// Used for the Authorization Code exchange
    AuthorizationCode(String),
    /// Used to refresh an access token
    RefreshToken(String)
}

/// The server's response to a successful token exchange, defined in
/// in RFC 6749 §5.1.
#[derive(Clone, PartialEq, Debug)]
#[derive(serde::Deserialize)]
pub struct TokenResponse {
    /// The access token issued by the authorization server.
    pub access_token: String,
    /// The type of token, described in RFC 6749 §7.1.
    pub token_type: String,
    /// The lifetime in seconds of the access token, if the authorization server
    /// provided one.
    pub expires_in: Option<i32>,
    /// The refresh token, if the server provided one.
    pub refresh_token: Option<String>,
    /// The (space-separated) list of scopes associated with the access token.
    /// The authorization server is required to provide this if it differs from
    /// the requested set of scopes.
    pub scope: Option<String>,

    /// Additional values returned by the authorization server, if any.
    #[serde(flatten)]
    pub extras: HashMap<String, JsonValue>,
}

/// An OAuth2 `Adapater` can be implemented by any type that facilitates the
/// Authorization Code Grant as described in RFC 6749 §4.1. The implementing
/// type must be able to generate an authorization URI and perform the token
/// exchange.
pub trait Adapter: Send + Sync + 'static {
    /// The `Error` type returned by this `Adapter` when a URI generation or
    /// token exchange fails.
    type Error: Debug;

    /// Generate an authorization URI and state value as described by RFC 6749 §4.1.1.
    fn authorization_uri(
        &self,
        config: &OAuthConfig,
        scopes: &[&str],
    ) -> Result<(Absolute<'static>, String), Self::Error>;

    /// Perform the token exchange in accordance with RFC 6749 §4.1.3 given the
    /// authorization code provided by the service.
    fn exchange_code(&self, config: &OAuthConfig, token: TokenRequest)
        -> Result<TokenResponse, Self::Error>;
}

/// An OAuth2 `Callback` implements application-specific OAuth client logic,
/// such as setting login cookies and making database and API requests. It is
/// tied to a specific `Adapter`, and will recieve an instance of the Adapter's
/// `Token` type.
pub trait Callback: Send + Sync + 'static {
    // TODO: Relax 'static. Would this need GAT/ATC?
    /// The callback Responder type.
    type Responder: Responder<'static>;

    /// This method will be called when a token exchange has successfully
    /// completed and will be provided with the request and the token.
    /// Implementors should perform application-specific logic here, such as
    /// checking a database or setting a login cookie.
    fn callback(&self, request: &Request<'_>, token: TokenResponse) -> Self::Responder;
}

impl<F, R> Callback for F
where
    F: Fn(&Request<'_>, TokenResponse) -> R + Send + Sync + 'static,
    R: Responder<'static>,
{
    type Responder = R;

    fn callback(&self, request: &Request<'_>, token: TokenResponse) -> Self::Responder {
        (self)(request, token)
    }
}

/// The `OAuth2` structure implements OAuth in a Rocket application by setting
/// up OAuth-related route handlers.
///
/// ## Redirect handler
/// `OAuth2` handles the redirect URI. It verifies the `state` token to prevent
/// CSRF attacks, then instructs the Adapter to perform the token exchange. The
/// resulting token is passed to the `Callback`.
///
/// ## Login handler
/// `OAuth2` optionally handles a login route, which simply redirects to the
/// authorization URI generated by the `Adapter`. Whether or not `OAuth2` is
/// handling a login URI, `get_redirect` can be used to get a `Redirect` to the
/// OAuth login flow manually.
#[derive(Clone, Debug)]
pub struct OAuth2<A, C> {
    adapter: A,
    callback: C,
    config: OAuthConfig,
    login_scopes: Vec<String>,
}

impl<A: Adapter, C: Callback> OAuth2<A, C> {
    /// Returns an OAuth2 fairing. The fairing will place an instance of
    /// `OAuth2<A, C>` in managed state and mount a redirect handler. It will
    /// also mount a login handler if `login` is `Some`.
    pub fn fairing(
        adapter: A,
        callback: C,
        config_name: &str,
        callback_uri: &str,
        login: Option<(&str, Vec<String>)>,
    ) -> impl Fairing {
        // Unfortunate allocations, but necessary because on_attach requires 'static
        let config_name = config_name.to_string();
        let callback_uri = callback_uri.to_string();
        let mut login = login.map(|(lu, ls)| (lu.to_string(), ls));

        AdHoc::on_attach("OAuth Init", move |rocket| {
            let config = match OAuthConfig::from_config(rocket.config(), &config_name) {
                Ok(c) => c,
                Err(e) => {
                    log::error!("Invalid configuration: {:?}", e);
                    return Err(rocket);
                }
            };

            let mut new_login = None;
            if let Some((lu, ls)) = login.as_mut() {
                let new_ls = std::mem::replace(ls, vec![]);
                new_login = Some((lu.as_str(), new_ls));
            };

            Ok(rocket.attach(Self::custom(
                adapter,
                callback,
                config,
                &callback_uri,
                new_login,
            )))
        })
    }

    /// Returns an OAuth2 fairing with custom configuration. The fairing will
    /// place an instance of `OAuth2<A, C>` in managed state and mount a
    /// redirect handler. It will also mount a login handler if `login` is
    /// `Some`.
    pub fn custom(
        adapter: A,
        callback: C,
        config: OAuthConfig,
        callback_uri: &str,
        login: Option<(&str, Vec<String>)>,
    ) -> impl Fairing {
        let mut routes = Vec::new();

        routes.push(Route::new(
            Method::Get,
            callback_uri,
            redirect_handler::<A, C>,
        ));

        let mut login_scopes = vec![];
        if let Some((uri, scopes)) = login {
            routes.push(Route::new(Method::Get, uri, login_handler::<A, C>));
            login_scopes = scopes;
        }

        let oauth2 = Self {
            adapter,
            callback,
            config,
            login_scopes,
        };

        AdHoc::on_attach("OAuth Mount", |rocket| {
            Ok(rocket.manage(oauth2).mount("/", routes))
        })
    }

    /// Prepare an authentication redirect. This sets a state cookie and returns
    /// a `Redirect` to the provider's authorization page.
    pub fn get_redirect(
        &self,
        cookies: &mut Cookies<'_>,
        scopes: &[&str],
    ) -> Result<Redirect, A::Error> {
        let (uri, state) = self.adapter.authorization_uri(&self.config, scopes)?;
        cookies.add_private(
            Cookie::build(STATE_COOKIE_NAME, state.clone())
                .same_site(SameSite::Lax)
                .finish(),
        );
        Ok(Redirect::to(uri))
    }

    /// Request a new access token given a refresh token. The refresh token
    /// must have been returned by the provider in a previous [`TokenResponse`].
    pub fn refresh(&self, refresh_token: &str) -> Result<TokenResponse, A::Error> {
        self.adapter.exchange_code(&self.config, TokenRequest::RefreshToken(refresh_token.to_string()))
    }

    // TODO: Decide if BadRequest is the appropriate error code.
    // TODO: What do providers do if they *reject* the authorization?
    /// Handle the redirect callback, delegating to the adapter and callback to
    /// perform the token exchange and application-specific actions.
    fn handle<'r>(&self, request: &'r Request<'_>, _data: Data) -> handler::Outcome<'r> {
        // Parse the query data.
        let query = request.uri().query().into_outcome(Status::BadRequest)?;

        #[derive(FromForm)]
        struct CallbackQuery {
            code: String,
            state: String,
            // Nonstandard (but see below)
            scope: Option<String>
        }

        let params = match CallbackQuery::from_form(&mut FormItems::from(query), false) {
            Ok(p) => p,
            Err(_) => return handler::Outcome::failure(Status::BadRequest),
        };

        {
            // Verify that the given state is the same one in the cookie.
            // Begin a new scope so that cookies is not kept around too long.
            let mut cookies = request.guard::<Cookies<'_>>().expect("request cookies");
            match cookies.get_private(STATE_COOKIE_NAME) {
                Some(ref cookie) if cookie.value() == params.state => {
                    cookies.remove(cookie.clone());
                }
                _ => return handler::Outcome::failure(Status::BadRequest),
            }
        }

        // Have the adapter perform the token exchange.
        let token = match self.adapter.exchange_code(&self.config, TokenRequest::AuthorizationCode(params.code)) {
            Ok(mut token) => {
                // Some providers (at least Strava) provide 'scope' in the callback
                // parameters instead of the token response as the RFC prescribes.
                // Therefore the 'scope' from the callback params is used as a fallback
                // if the token response does not specify one.
                if token.scope.is_none() {
                    token.scope = params.scope;
                }
                token
            },
            Err(e) => {
                log::error!("Token exchange failed: {:?}", e);
                return handler::Outcome::failure(Status::BadRequest);
            }
        };

        // Run the callback.
        let responder = self.callback.callback(request, token);
        handler::Outcome::from(request, responder)
    }
}

// These cannot be closures becuase of the lifetime parameter.
// TODO: cross-reference rust-lang/rust issues.

/// Handles the OAuth redirect route
fn redirect_handler<'r, A: Adapter, C: Callback>(
    request: &'r Request<'_>,
    data: Data,
) -> handler::Outcome<'r> {
    let oauth = match request.guard::<State<'_, OAuth2<A, C>>>() {
        Outcome::Success(oauth) => oauth,
        Outcome::Failure(_) => return handler::Outcome::failure(Status::InternalServerError),
        Outcome::Forward(()) => unreachable!(),
    };
    oauth.handle(request, data)
}

/// Handles a login route, performing a redirect
fn login_handler<'r, A: Adapter, C: Callback>(
    request: &'r Request<'_>,
    _data: Data,
) -> handler::Outcome<'r> {
    let oauth = match request.guard::<State<'_, OAuth2<A, C>>>() {
        Outcome::Success(oauth) => oauth,
        Outcome::Failure(_) => return handler::Outcome::failure(Status::InternalServerError),
        Outcome::Forward(()) => unreachable!(),
    };
    let mut cookies = request.guard::<Cookies<'_>>().expect("request cookies");
    let scopes: Vec<_> = oauth.login_scopes.iter().map(String::as_str).collect();
    handler::Outcome::from(request, oauth.get_redirect(&mut cookies, &scopes))
}
