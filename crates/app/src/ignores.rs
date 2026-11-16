//! Ignored users and phrases in the app (PR 5.7).
//!
//! [`IgnoreHub`] is the one place that knows what is ignored: the `[ignores]` and `[filters]`
//! sections of the settings, the Twitch block list when the login may read it, and who the local
//! user is (their own messages are never ignored). It keeps them compiled
//! ([`gpui_chat_core::Ignores`], shared as an `Arc`) and tells its observers when the answer
//! changed: the chat views (rows come and go), [`crate::attention::Attention`] (an ignored message
//! counts for no dot, no Mentions entry and no notification) and the settings page.
//!
//! The hub is *not* applied to messages as they arrive: it is asked about a message whenever the
//! message is drawn or counted, so ignoring a user hides what they already said, and unignoring
//! brings it back (it is still in the scrollback). Messages that arrived while a user was not
//! ignored have been counted as unread by then; that is not taken back.
//!
//! **The Twitch block list** (`GET /users/blocks`, scope `user:read:blocked_users`) is fetched
//! when a login that was granted the scope is there and `ignores.sync_block_list` is on, and again
//! after the user blocks or unblocks someone from the app. The scopes are opt-in like the
//! moderation ones (*Enable block list sync…*); a login without them simply has no synced list
//! ([`BlockStatus::NeedsScope`]), nothing is requested and nothing fails.

use std::sync::Arc;
use std::time::Duration;

use gpui_chat_core::{BotList, Ignores, ignore_user_problem};
use gpui_chat_settings::{EditValue, SettingEdit};
use gpui_chat_twitch::{BlockError, BlockedUser};
use gpui_kit::{App, AppContext as _, Context, Entity, Global, Subscription, Task};
use tokio::sync::oneshot;

use crate::account;
use crate::config;
use crate::runtime;

/// Where the synced Twitch block list stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockStatus {
    /// Not used: nobody is logged in (or this is an offline run), or `ignores.sync_block_list` is
    /// off.
    Off,
    /// The login was not granted the block list scopes: *Enable block list sync…* asks for them.
    NeedsScope,
    /// Being fetched.
    Loading,
    /// Fetched: this many users are blocked.
    Ready(usize),
    /// The last fetch failed (the sentence says why); the previous list, if any, stays in force.
    Failed(String),
}

impl BlockStatus {
    /// What the settings page says.
    pub fn describe(&self) -> String {
        match self {
            Self::Off => "Not in use (log in, and keep “Use my Twitch block list” on).".to_owned(),
            Self::NeedsScope => {
                "Needs extra Twitch permissions: use “Enable block list sync…”.".to_owned()
            }
            Self::Loading => "Loading your block list…".to_owned(),
            Self::Ready(0) => "Synced: nobody is blocked.".to_owned(),
            Self::Ready(1) => "Synced: 1 blocked user is ignored.".to_owned(),
            Self::Ready(n) => format!("Synced: {n} blocked users are ignored."),
            Self::Failed(why) => {
                format!("Could not load your block list ({why}). Trying again shortly.")
            }
        }
    }
}

/// How long to wait before asking for the block list again after a failure: 30 s, doubling to 15 min.
fn backoff(failures: u32) -> Duration {
    Duration::from_secs(30)
        .saturating_mul(1 << failures.saturating_sub(1).min(5))
        .min(REFRESH)
}

/// How often the synced block list is fetched again while it is in use, to pick up what was blocked
/// or unblocked on twitch.tv.
const REFRESH: Duration = Duration::from_secs(15 * 60);

/// What the hub depends on in the account: when this changes, who is ignored may have.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct AccountKey {
    own: Option<String>,
    user_id: Option<String>,
    block_list: bool,
}

/// What is ignored, for the whole app. One per app, created at startup ([`init`]).
pub struct IgnoreHub {
    /// The offline sources (`--firehose`, `--replay`) act as [`crate::attention::OFFLINE_LOGIN`].
    offline: bool,
    ignores: gpui_chat_core::IgnoreConfig,
    filters: gpui_chat_core::FilterConfig,
    engine: Arc<Ignores>,
    bots: Arc<BotList>,
    blocked: Vec<BlockedUser>,
    /// The account `blocked` was fetched for.
    blocked_for: Option<String>,
    status: BlockStatus,
    /// Bumped on every change of what is ignored (or of the bot list); views compare it.
    generation: u64,
    /// Numbers block list fetches so a stale answer is dropped.
    fetch: u64,
    _fetching: Option<Task<()>>,
    /// Fetches the block list again: after a failure (backoff) or on the long refresh interval.
    _timer: Option<Task<()>>,
    /// Failed fetches in a row.
    failures: u32,
    account_key: AccountKey,
    _subscriptions: Vec<Subscription>,
}

struct Handle(Entity<IgnoreHub>);

impl Global for Handle {}

/// Creates the hub (once, at startup, after the account hub if there is one).
pub fn init(offline: bool, cx: &mut App) -> Entity<IgnoreHub> {
    let hub = cx.new(|cx| IgnoreHub::new(offline, cx));
    cx.set_global(Handle(hub.clone()));
    hub
}

/// The app's hub, or `None` before [`init`] (tests that do not build one).
pub fn current(cx: &App) -> Option<Entity<IgnoreHub>> {
    cx.try_global::<Handle>().map(|handle| handle.0.clone())
}

impl IgnoreHub {
    fn new(offline: bool, cx: &mut Context<Self>) -> Self {
        let settings = config::current(cx);
        let mut hub = Self {
            offline,
            ignores: settings.ignores.clone(),
            filters: settings.filters.clone(),
            engine: Arc::default(),
            bots: Arc::default(),
            blocked: Vec::new(),
            blocked_for: None,
            status: BlockStatus::Off,
            generation: 0,
            fetch: 0,
            _fetching: None,
            _timer: None,
            failures: 0,
            account_key: AccountKey::default(),
            _subscriptions: Vec::new(),
        };
        hub.account_key = hub.account_key(cx);
        hub.rebuild(cx);
        let mut subscriptions = vec![cx.observe(&config::store(cx), |this, _, cx| {
            let settings = config::current(cx);
            if settings.ignores != this.ignores || settings.filters != this.filters {
                this.ignores = settings.ignores.clone();
                this.filters = settings.filters.clone();
                this.rebuild(cx);
                this.sync_block_list(false, cx);
            }
        })];
        if let Some(account) = account::current(cx) {
            // Logging in or out changes who the user is and which block list applies.
            // (The hub notifies for every step of a sign-in too: only a change of who the user is, or
            // of what the login may do, matters here.)
            subscriptions.push(cx.observe(&account, |this, _, cx| {
                let key = this.account_key(cx);
                if key != this.account_key {
                    this.account_key = key;
                    this.rebuild(cx);
                    this.sync_block_list(false, cx);
                }
            }));
        }
        hub._subscriptions = subscriptions;
        hub.sync_block_list(false, cx);
        hub
    }

    /// What is ignored, compiled. Cheap to clone.
    pub fn engine(&self) -> Arc<Ignores> {
        self.engine.clone()
    }

    /// The bots *Hide bots* hides.
    pub fn bots(&self) -> Arc<BotList> {
        self.bots.clone()
    }

    /// Changes whenever the answer to "is this ignored" (or the bot list) may have.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn status(&self) -> &BlockStatus {
        &self.status
    }

    /// Whether `login` is on the Twitch block list.
    pub fn is_blocked(&self, login: &str) -> bool {
        self.engine.is_blocked(login)
    }

    /// Whether `login` is ignored by `[ignores] users` (what *Unignore user* removes).
    pub fn is_listed(&self, login: &str) -> bool {
        self.ignores.ignores_user(login)
    }

    fn account_key(&self, cx: &App) -> AccountKey {
        let block = Self::block_list_enabled(cx);
        AccountKey {
            own: self.own_login(cx),
            user_id: block.as_ref().map(|(_, id)| id.clone()),
            block_list: block.is_some_and(|(enabled, _)| enabled),
        }
    }

    /// Whether `login` is the local user.
    pub fn is_own(&self, login: &str, cx: &App) -> bool {
        self.own_login(cx)
            .is_some_and(|own| own.eq_ignore_ascii_case(login.trim().trim_start_matches('@')))
    }

    /// Why `login` cannot be ignored or blocked from here (the user themselves, or not a login).
    pub fn can_target(&self, login: &str, cx: &App) -> Result<(), String> {
        if self.is_own(login, cx) {
            return Err("You cannot do that to yourself.".to_owned());
        }
        match ignore_user_problem(login) {
            Some(problem) => Err(problem),
            None => Ok(()),
        }
    }

    /// The local user: the account, or the offline stand-in.
    fn own_login(&self, cx: &App) -> Option<String> {
        if self.offline {
            Some(crate::attention::OFFLINE_LOGIN.to_owned())
        } else {
            account::own_login(cx)
        }
    }

    /// Compiles the settings and the block list again and tells the observers.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let (mut engine, problems) = Ignores::compile(&self.ignores);
        // A bad entry is left out and reported by the settings window; the rest still apply.
        for problem in &problems {
            tracing::debug!(%problem, "ignore entry left out");
        }
        engine.set_own(self.own_login(cx).as_deref());
        engine.set_blocked(
            self.blocked
                .iter()
                .map(|user| (user.id.clone(), user.login.clone())),
        );
        self.engine = Arc::new(engine);
        self.bots = Arc::new(BotList::new(&self.filters));
        self.generation += 1;
        cx.notify();
    }

    /// Adds `login` to `[ignores] users` (written to `settings.toml`; it takes effect at once).
    pub fn ignore_user(&mut self, login: &str, cx: &mut Context<Self>) -> Result<(), String> {
        let login = gpui_chat_core::highlight::normalise_user(login);
        self.can_target(&login, cx)?;
        if self.is_listed(&login) {
            return Ok(());
        }
        let mut users = self.ignores.users.clone();
        users.push(login);
        write_users(users, cx)
    }

    /// Removes `login` from `[ignores] users`. A user who is only on the Twitch block list is
    /// still ignored (that needs *Unblock on Twitch*).
    pub fn unignore_user(&mut self, login: &str, cx: &mut Context<Self>) -> Result<(), String> {
        if !self.is_listed(login) {
            return Ok(());
        }
        let login = gpui_chat_core::highlight::normalise_user(login);
        let users: Vec<String> = self
            .ignores
            .users
            .iter()
            .filter(|user| gpui_chat_core::highlight::normalise_user(user) != login)
            .cloned()
            .collect();
        write_users(users, cx)
    }

    /// The scopes needed to read and change the block list were granted to the logged-in account.
    fn block_list_enabled(cx: &App) -> Option<(bool, String)> {
        let hub = account::current(cx)?;
        let hub = hub.read(cx);
        let account = hub.state().account()?;
        Some((account.block_list_enabled(), account.user_id.clone()))
    }

    /// Fetches the Twitch block list when it should be used and is not up to date (`force`: even
    /// if it was fetched for this account already).
    pub fn sync_block_list(&mut self, force: bool, cx: &mut Context<Self>) {
        let wanted = self.ignores.sync_block_list && !self.offline;
        let account = Self::block_list_enabled(cx);
        let target = match (&account, wanted) {
            (Some((true, user_id)), true) => Ok(user_id.clone()),
            (Some((false, _)), true) => Err(BlockStatus::NeedsScope),
            _ => Err(BlockStatus::Off),
        };
        let user_id = match target {
            Ok(user_id) => user_id,
            Err(status) => {
                // Nothing to use: forget what another account (or an earlier setting) had.
                self.fetch += 1;
                self._fetching = None;
                self._timer = None;
                self.failures = 0;
                let changed = !self.blocked.is_empty() || self.status != status;
                self.blocked.clear();
                self.blocked_for = None;
                self.status = status;
                if changed {
                    self.rebuild(cx);
                }
                return;
            }
        };
        if !force && self.blocked_for.as_deref() == Some(user_id.as_str()) {
            return;
        }
        if !force && self.status == BlockStatus::Loading {
            return;
        }
        let Some(helix) = crate::user_directory::helix(cx) else {
            return;
        };
        // Another account's list never applies to this one, not even while its own loads.
        if self.blocked_for.as_deref() != Some(user_id.as_str())
            && (!self.blocked.is_empty() || self.blocked_for.is_some())
        {
            self.blocked.clear();
            self.blocked_for = None;
            self.rebuild(cx);
        }
        self.fetch += 1;
        let epoch = self.fetch;
        self._timer = None;
        self.status = BlockStatus::Loading;
        let (done, outcome) = oneshot::channel();
        drop(runtime::spawn_backend(cx, async move {
            let _ = done.send(helix.get_blocked_users().await);
        }));
        self._fetching = Some(cx.spawn(async move |this, cx| {
            let Ok(result) = outcome.await else {
                return;
            };
            this.update(cx, |this, cx| {
                if this.fetch != epoch {
                    return;
                }
                match result {
                    Ok(users) => {
                        this.status = BlockStatus::Ready(users.len());
                        this.blocked = users;
                        this.blocked_for = Some(user_id);
                        this.failures = 0;
                        this.schedule_refresh(REFRESH, cx);
                    }
                    Err(error) => {
                        tracing::info!(%error, "could not load the Twitch block list");
                        if error.is_missing_scope() {
                            this.status = BlockStatus::NeedsScope;
                        } else {
                            this.status = BlockStatus::Failed(error.to_string());
                            this.failures += 1;
                            this.schedule_refresh(backoff(this.failures), cx);
                        }
                    }
                }
                this.rebuild(cx);
            })
            .ok();
        }));
        cx.notify();
    }

    /// Fetches the block list again in `delay` (replacing a fetch already due).
    fn schedule_refresh(&mut self, delay: Duration, cx: &mut Context<Self>) {
        self._timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |this, cx| this.sync_block_list(true, cx))
                .ok();
        }));
    }

    /// Fetches the block list now (*Refresh* on the Ignores page, and the retry after a failure).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.sync_block_list(true, cx);
    }

    /// Blocks `login` on Twitch (`user_id` when the message supplied it, else looked up first) and
    /// adds them to the synced list. The task's result is the sentence to show when it failed
    /// (a login without the scopes says to enable block list sync).
    pub fn block(
        &mut self,
        login: String,
        user_id: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), BlockError>> {
        self.change_block(login, user_id, true, cx)
    }

    /// Unblocks `login` on Twitch.
    pub fn unblock(
        &mut self,
        login: String,
        user_id: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), BlockError>> {
        self.change_block(login, user_id, false, cx)
    }

    fn change_block(
        &mut self,
        login: String,
        user_id: Option<String>,
        block: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), BlockError>> {
        let Some(helix) = crate::user_directory::helix(cx) else {
            return Task::ready(Err(BlockError::Helix(gpui_chat_twitch::HelixError::Auth(
                gpui_chat_twitch::auth::AuthError::NotLoggedIn,
            ))));
        };
        // Never the user themselves, by name or by account id.
        let own_id = account_id(cx);
        if self.is_own(&login, cx) || (user_id.is_some() && user_id == own_id) {
            return Task::ready(Err(BlockError::Rejected {
                status: 0,
                message: "You cannot block yourself.".to_owned(),
            }));
        }
        // The id of an account is what Twitch blocks; the one the message carried is the account
        // that sent it (a name may have moved to another account since).
        let known = user_id.or_else(|| {
            self.blocked
                .iter()
                .find(|user| user.login == login)
                .map(|user| user.id.clone())
        });
        let (done, outcome) = oneshot::channel();
        let asked = login.clone();
        drop(runtime::spawn_backend(cx, async move {
            let result = async {
                let id = match known {
                    Some(id) => id,
                    None => helix
                        .get_user(gpui_chat_twitch::UserQuery::Login(&asked))
                        .await
                        .map_err(|error| BlockError::Rejected {
                            status: 0,
                            message: format!("Could not look {asked} up ({error})."),
                        })?
                        .map(|profile| profile.id)
                        .ok_or_else(|| BlockError::Rejected {
                            status: 404,
                            message: format!("No Twitch user is called {asked}."),
                        })?,
                };
                if block {
                    helix.block_user(&id).await?;
                } else {
                    helix.unblock_user(&id).await?;
                }
                Ok::<_, BlockError>(id)
            }
            .await;
            let _ = done.send(result);
        }));
        cx.spawn(async move |this, cx| {
            let Ok(result) = outcome.await else {
                return Ok(());
            };
            let id = result?;
            this.update(cx, |this, cx| {
                // The list is only touched while it is in use and belongs to the account that
                // blocked (a list another account, or nobody, had must not take this on); with
                // sync on and nothing loaded yet, the next fetch picks the change up.
                if !this.ignores.sync_block_list || this.blocked_for.is_none() {
                    if this.ignores.sync_block_list {
                        this.sync_block_list(true, cx);
                    }
                    return;
                }
                this.blocked.retain(|user| user.id != id);
                if block {
                    this.blocked.push(BlockedUser {
                        id,
                        login: login.to_lowercase(),
                        display_name: login,
                    });
                }
                if matches!(this.status, BlockStatus::Ready(_)) {
                    this.status = BlockStatus::Ready(this.blocked.len());
                }
                this.rebuild(cx);
            })
            .ok();
            Ok(())
        })
    }
}

/// The user id of the logged-in account.
fn account_id(cx: &App) -> Option<String> {
    let hub = account::current(cx)?;
    hub.read(cx)
        .state()
        .account()
        .map(|account| account.user_id.clone())
}

fn write_users(users: Vec<String>, cx: &mut Context<IgnoreHub>) -> Result<(), String> {
    config::edit(
        &[SettingEdit::set("ignores.users", EditValue::StrList(users))],
        cx,
    )
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use gpui_chat_core::{Author, Fragment, IgnoreConfig, Message, Timestamp};
    use gpui_chat_settings::{Settings, SettingsEvent};
    use gpui_kit::TestAppContext;

    use super::*;

    fn said(login: &str, text: &str) -> Message {
        Message::chat(
            "id",
            Timestamp(0),
            Author::new(login, login),
            vec![Fragment::Text(text.into())],
        )
    }

    fn settings_with(ignores: IgnoreConfig) -> Settings {
        Settings {
            ignores,
            ..Settings::default()
        }
    }

    fn start(settings: Settings, cx: &mut TestAppContext) -> Entity<IgnoreHub> {
        cx.update(|cx| {
            config::install_for_test(settings, cx);
            init(true, cx)
        })
    }

    #[gpui_kit::test]
    fn the_settings_decide_what_is_ignored_and_a_change_is_noticed(cx: &mut TestAppContext) {
        let hub = start(
            settings_with(IgnoreConfig {
                users: vec!["spam".into()],
                ..IgnoreConfig::default()
            }),
            cx,
        );
        let (engine, generation) = hub.read_with(cx, |hub, _| (hub.engine(), hub.generation()));
        assert!(engine.is_ignored(&said("spam", "x")));
        assert!(!engine.is_ignored(&said("fine", "x")));
        assert!(!engine.is_ignored(&said(crate::attention::OFFLINE_LOGIN, "x")));

        // The file changes (the watcher's event): the hub recompiles and says so.
        cx.update(|cx| {
            config::store(cx).update(cx, |store, cx| {
                let settings = settings_with(IgnoreConfig {
                    users: vec!["other".into()],
                    ..IgnoreConfig::default()
                });
                if store.apply_events([SettingsEvent::Reloaded(settings)]) {
                    cx.notify();
                }
            });
        });
        let (engine, later) = hub.read_with(cx, |hub, _| (hub.engine(), hub.generation()));
        assert!(later > generation);
        assert!(engine.is_ignored(&said("other", "x")));
        assert!(!engine.is_ignored(&said("spam", "x")));
    }

    #[gpui_kit::test]
    fn an_unrelated_settings_change_does_not_recompile_anything(cx: &mut TestAppContext) {
        let hub = start(Settings::default(), cx);
        let before = hub.read_with(cx, |hub, _| hub.generation());
        cx.update(|cx| {
            config::store(cx).update(cx, |store, cx| {
                let mut settings = Settings::default();
                settings.chat.timestamps = false;
                if store.apply_events([SettingsEvent::Reloaded(settings)]) {
                    cx.notify();
                }
            });
        });
        assert_eq!(hub.read_with(cx, |hub, _| hub.generation()), before);
    }

    #[gpui_kit::test]
    fn the_bot_list_and_the_own_login_come_from_the_settings_and_the_run(cx: &mut TestAppContext) {
        let mut settings = Settings::default();
        settings.filters.bots = vec!["mybot".into()];
        settings.ignores.users = vec![crate::attention::OFFLINE_LOGIN.into()];
        let hub = start(settings, cx);
        hub.read_with(cx, |hub, _| {
            assert!(hub.bots().contains("MyBot") && !hub.bots().contains("nightbot"));
            // The offline user's own messages are never ignored, even when listed.
            assert!(
                !hub.engine()
                    .is_ignored(&said(crate::attention::OFFLINE_LOGIN, "hi"))
            );
            assert!(hub.is_listed(crate::attention::OFFLINE_LOGIN));
        });
    }

    #[gpui_kit::test]
    fn without_a_login_the_block_list_is_not_used(cx: &mut TestAppContext) {
        let hub = start(Settings::default(), cx);
        hub.read_with(cx, |hub, _| {
            assert_eq!(*hub.status(), BlockStatus::Off);
            assert!(!hub.is_blocked("anyone"));
        });
    }

    #[gpui_kit::test]
    fn the_user_cannot_be_ignored_or_blocked(cx: &mut TestAppContext) {
        let hub = start(Settings::default(), cx);
        let result = hub.update(cx, |hub, cx| {
            hub.ignore_user(crate::attention::OFFLINE_LOGIN, cx)
        });
        assert!(result.unwrap_err().contains("yourself"));
        let result = hub.update(cx, |hub, cx| hub.ignore_user("@Viewer", cx));
        assert!(result.is_err(), "whatever the capitals");
        hub.read_with(cx, |hub, cx| {
            assert!(hub.is_own("VIEWER", cx) && !hub.is_own("someone", cx));
            assert!(hub.can_target("someone", cx).is_ok());
            assert!(hub.can_target("viewer", cx).is_err());
            assert!(hub.can_target("not a login", cx).is_err());
        });
    }

    #[test]
    fn a_failed_block_list_is_asked_again_with_growing_pauses_up_to_the_refresh_interval() {
        let pauses: Vec<u64> = (1..=8).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(pauses, [30, 60, 120, 240, 480, 900, 900, 900]);
        assert_eq!(backoff(0).as_secs(), 30);
        assert_eq!(REFRESH.as_secs(), 900);
    }

    #[test]
    fn the_status_says_what_to_do() {
        assert!(
            BlockStatus::NeedsScope
                .describe()
                .contains("Enable block list sync")
        );
        assert_eq!(
            BlockStatus::Ready(2).describe(),
            "Synced: 2 blocked users are ignored."
        );
        assert!(
            BlockStatus::Failed("x".into())
                .describe()
                .contains("Could not load")
        );
    }
}
