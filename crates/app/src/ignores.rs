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
            Self::Failed(why) => format!("Could not load your block list ({why})"),
        }
    }
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
            _subscriptions: Vec::new(),
        };
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
            subscriptions.push(cx.observe(&account, |this, _, cx| {
                this.rebuild(cx);
                this.sync_block_list(false, cx);
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
        if let Some(problem) = ignore_user_problem(&login) {
            return Err(problem);
        }
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
        self.fetch += 1;
        let epoch = self.fetch;
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
                    }
                    Err(error) => {
                        tracing::info!(%error, "could not load the Twitch block list");
                        this.status = if error.is_missing_scope() {
                            BlockStatus::NeedsScope
                        } else {
                            BlockStatus::Failed(error.to_string())
                        };
                    }
                }
                this.rebuild(cx);
            })
            .ok();
        }));
        cx.notify();
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
