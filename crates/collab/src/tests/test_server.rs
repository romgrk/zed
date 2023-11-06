use crate::{
    db::{tests::TestDb, NewUserParams, UserId},
    executor::Executor,
    rpc::{Server, CLEANUP_TIMEOUT, RECONNECT_TIMEOUT},
    AppState,
};
use anyhow::anyhow;
use call::ActiveCall;
use channel::{ChannelBuffer, ChannelStore};
use client::{
    self, proto::PeerId, Client, Connection, Credentials, EstablishConnectionError, UserStore,
};
use collections::{HashMap, HashSet};
use fs::FakeFs;
use futures::{channel::oneshot, StreamExt as _};
use gpui::{executor::Deterministic, ModelHandle, Task, TestAppContext, WindowHandle};
use language::LanguageRegistry;
use node_runtime::FakeNodeRuntime;
use notifications::NotificationStore;
use parking_lot::Mutex;
use project::{Project, WorktreeId};
use rpc::{proto::ChannelRole, RECEIVE_TIMEOUT};
use settings::SettingsStore;
use std::{
    cell::{Ref, RefCell, RefMut},
    env,
    ops::{Deref, DerefMut},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
        Arc,
    },
};
use util::http::FakeHttpClient;
use workspace::{Workspace, WorkspaceStore};

pub struct TestServer {
    pub app_state: Arc<AppState>,
    pub test_live_kit_server: Arc<live_kit_client::TestServer>,
    server: Arc<Server>,
    connection_killers: Arc<Mutex<HashMap<PeerId, Arc<AtomicBool>>>>,
    forbid_connections: Arc<AtomicBool>,
    _test_db: TestDb,
}

pub struct TestClient {
    pub username: String,
    pub app_state: Arc<workspace::AppState>,
    channel_store: ModelHandle<ChannelStore>,
    notification_store: ModelHandle<NotificationStore>,
    state: RefCell<TestClientState>,
}

#[derive(Default)]
struct TestClientState {
    local_projects: Vec<ModelHandle<Project>>,
    remote_projects: Vec<ModelHandle<Project>>,
    buffers: HashMap<ModelHandle<Project>, HashSet<ModelHandle<language::Buffer>>>,
    channel_buffers: HashSet<ModelHandle<ChannelBuffer>>,
}

pub struct ContactsSummary {
    pub current: Vec<String>,
    pub outgoing_requests: Vec<String>,
    pub incoming_requests: Vec<String>,
}

impl TestServer {
    pub async fn start(deterministic: &Arc<Deterministic>) -> Self {
        static NEXT_LIVE_KIT_SERVER_ID: AtomicUsize = AtomicUsize::new(0);

        let use_postgres = env::var("USE_POSTGRES").ok();
        let use_postgres = use_postgres.as_deref();
        let test_db = if use_postgres == Some("true") || use_postgres == Some("1") {
            TestDb::postgres(deterministic.build_background())
        } else {
            TestDb::sqlite(deterministic.build_background())
        };
        let live_kit_server_id = NEXT_LIVE_KIT_SERVER_ID.fetch_add(1, SeqCst);
        let live_kit_server = live_kit_client::TestServer::create(
            format!("http://livekit.{}.test", live_kit_server_id),
            format!("devkey-{}", live_kit_server_id),
            format!("secret-{}", live_kit_server_id),
            deterministic.build_background(),
        )
        .unwrap();
        let app_state = Self::build_app_state(&test_db, &live_kit_server).await;
        let epoch = app_state
            .db
            .create_server(&app_state.config.zed_environment)
            .await
            .unwrap();
        let server = Server::new(
            epoch,
            app_state.clone(),
            Executor::Deterministic(deterministic.build_background()),
        );
        server.start().await.unwrap();
        // Advance clock to ensure the server's cleanup task is finished.
        deterministic.advance_clock(CLEANUP_TIMEOUT);
        Self {
            app_state,
            server,
            connection_killers: Default::default(),
            forbid_connections: Default::default(),
            _test_db: test_db,
            test_live_kit_server: live_kit_server,
        }
    }

    pub async fn reset(&self) {
        self.app_state.db.reset();
        let epoch = self
            .app_state
            .db
            .create_server(&self.app_state.config.zed_environment)
            .await
            .unwrap();
        self.server.reset(epoch);
    }

    pub async fn create_client(&mut self, cx: &mut TestAppContext, name: &str) -> TestClient {
        cx.update(|cx| {
            if cx.has_global::<SettingsStore>() {
                panic!("Same cx used to create two test clients")
            }

            cx.set_global(SettingsStore::test(cx));
        });

        let http = FakeHttpClient::with_404_response();
        let user_id = if let Ok(Some(user)) = self.app_state.db.get_user_by_github_login(name).await
        {
            user.id
        } else {
            self.app_state
                .db
                .create_user(
                    &format!("{name}@example.com"),
                    false,
                    NewUserParams {
                        github_login: name.into(),
                        github_user_id: 0,
                    },
                )
                .await
                .expect("creating user failed")
                .user_id
        };
        let client_name = name.to_string();
        let mut client = cx.read(|cx| Client::new(http.clone(), cx));
        let server = self.server.clone();
        let db = self.app_state.db.clone();
        let connection_killers = self.connection_killers.clone();
        let forbid_connections = self.forbid_connections.clone();

        Arc::get_mut(&mut client)
            .unwrap()
            .set_id(user_id.to_proto())
            .override_authenticate(move |cx| {
                cx.spawn(|_| async move {
                    let access_token = "the-token".to_string();
                    Ok(Credentials {
                        user_id: user_id.to_proto(),
                        access_token,
                    })
                })
            })
            .override_establish_connection(move |credentials, cx| {
                assert_eq!(credentials.user_id, user_id.0 as u64);
                assert_eq!(credentials.access_token, "the-token");

                let server = server.clone();
                let db = db.clone();
                let connection_killers = connection_killers.clone();
                let forbid_connections = forbid_connections.clone();
                let client_name = client_name.clone();
                cx.spawn(move |cx| async move {
                    if forbid_connections.load(SeqCst) {
                        Err(EstablishConnectionError::other(anyhow!(
                            "server is forbidding connections"
                        )))
                    } else {
                        let (client_conn, server_conn, killed) =
                            Connection::in_memory(cx.background());
                        let (connection_id_tx, connection_id_rx) = oneshot::channel();
                        let user = db
                            .get_user_by_id(user_id)
                            .await
                            .expect("retrieving user failed")
                            .unwrap();
                        cx.background()
                            .spawn(server.handle_connection(
                                server_conn,
                                client_name,
                                user,
                                Some(connection_id_tx),
                                Executor::Deterministic(cx.background()),
                            ))
                            .detach();
                        let connection_id = connection_id_rx.await.unwrap();
                        connection_killers
                            .lock()
                            .insert(connection_id.into(), killed);
                        Ok(client_conn)
                    }
                })
            });

        let fs = FakeFs::new(cx.background());
        let user_store = cx.add_model(|cx| UserStore::new(client.clone(), http, cx));
        let workspace_store = cx.add_model(|cx| WorkspaceStore::new(client.clone(), cx));
        let mut language_registry = LanguageRegistry::test();
        language_registry.set_executor(cx.background());
        let app_state = Arc::new(workspace::AppState {
            client: client.clone(),
            user_store: user_store.clone(),
            workspace_store,
            languages: Arc::new(language_registry),
            fs: fs.clone(),
            build_window_options: |_, _, _| Default::default(),
            initialize_workspace: |_, _, _, _| Task::ready(Ok(())),
            background_actions: || &[],
            node_runtime: FakeNodeRuntime::new(),
        });

        cx.update(|cx| {
            theme::init((), cx);
            Project::init(&client, cx);
            client::init(&client, cx);
            language::init(cx);
            editor::init_settings(cx);
            workspace::init(app_state.clone(), cx);
            audio::init((), cx);
            call::init(client.clone(), user_store.clone(), cx);
            channel::init(&client, user_store.clone(), cx);
            notifications::init(client.clone(), user_store, cx);
        });

        client
            .authenticate_and_connect(false, &cx.to_async())
            .await
            .unwrap();

        let client = TestClient {
            app_state,
            username: name.to_string(),
            channel_store: cx.read(ChannelStore::global).clone(),
            notification_store: cx.read(NotificationStore::global).clone(),
            state: Default::default(),
        };
        client.wait_for_current_user(cx).await;
        client
    }

    pub fn disconnect_client(&self, peer_id: PeerId) {
        self.connection_killers
            .lock()
            .remove(&peer_id)
            .unwrap()
            .store(true, SeqCst);
    }

    pub fn simulate_long_connection_interruption(
        &self,
        peer_id: PeerId,
        deterministic: &Arc<Deterministic>,
    ) {
        self.forbid_connections();
        self.disconnect_client(peer_id);
        deterministic.advance_clock(RECEIVE_TIMEOUT + RECONNECT_TIMEOUT);
        self.allow_connections();
        deterministic.advance_clock(RECEIVE_TIMEOUT + RECONNECT_TIMEOUT);
        deterministic.run_until_parked();
    }

    pub fn forbid_connections(&self) {
        self.forbid_connections.store(true, SeqCst);
    }

    pub fn allow_connections(&self) {
        self.forbid_connections.store(false, SeqCst);
    }

    pub async fn make_contacts(&self, clients: &mut [(&TestClient, &mut TestAppContext)]) {
        for ix in 1..clients.len() {
            let (left, right) = clients.split_at_mut(ix);
            let (client_a, cx_a) = left.last_mut().unwrap();
            for (client_b, cx_b) in right {
                client_a
                    .app_state
                    .user_store
                    .update(*cx_a, |store, cx| {
                        store.request_contact(client_b.user_id().unwrap(), cx)
                    })
                    .await
                    .unwrap();
                cx_a.foreground().run_until_parked();
                client_b
                    .app_state
                    .user_store
                    .update(*cx_b, |store, cx| {
                        store.respond_to_contact_request(client_a.user_id().unwrap(), true, cx)
                    })
                    .await
                    .unwrap();
            }
        }
    }

    pub async fn make_channel(
        &self,
        channel: &str,
        parent: Option<u64>,
        admin: (&TestClient, &mut TestAppContext),
        members: &mut [(&TestClient, &mut TestAppContext)],
    ) -> u64 {
        let (_, admin_cx) = admin;
        let channel_id = admin_cx
            .read(ChannelStore::global)
            .update(admin_cx, |channel_store, cx| {
                channel_store.create_channel(channel, parent, cx)
            })
            .await
            .unwrap();

        for (member_client, member_cx) in members {
            admin_cx
                .read(ChannelStore::global)
                .update(admin_cx, |channel_store, cx| {
                    channel_store.invite_member(
                        channel_id,
                        member_client.user_id().unwrap(),
                        ChannelRole::Member,
                        cx,
                    )
                })
                .await
                .unwrap();

            admin_cx.foreground().run_until_parked();

            member_cx
                .read(ChannelStore::global)
                .update(*member_cx, |channels, cx| {
                    channels.respond_to_channel_invite(channel_id, true, cx)
                })
                .await
                .unwrap();
        }

        channel_id
    }

    pub async fn make_channel_tree(
        &self,
        channels: &[(&str, Option<&str>)],
        creator: (&TestClient, &mut TestAppContext),
    ) -> Vec<u64> {
        let mut observed_channels = HashMap::default();
        let mut result = Vec::new();
        for (channel, parent) in channels {
            let id;
            if let Some(parent) = parent {
                if let Some(parent_id) = observed_channels.get(parent) {
                    id = self
                        .make_channel(channel, Some(*parent_id), (creator.0, creator.1), &mut [])
                        .await;
                } else {
                    panic!(
                        "Edge {}->{} referenced before {} was created",
                        parent, channel, parent
                    )
                }
            } else {
                id = self
                    .make_channel(channel, None, (creator.0, creator.1), &mut [])
                    .await;
            }

            observed_channels.insert(channel, id);
            result.push(id);
        }

        result
    }

    pub async fn create_room(&self, clients: &mut [(&TestClient, &mut TestAppContext)]) {
        self.make_contacts(clients).await;

        let (left, right) = clients.split_at_mut(1);
        let (_client_a, cx_a) = &mut left[0];
        let active_call_a = cx_a.read(ActiveCall::global);

        for (client_b, cx_b) in right {
            let user_id_b = client_b.current_user_id(*cx_b).to_proto();
            active_call_a
                .update(*cx_a, |call, cx| call.invite(user_id_b, None, cx))
                .await
                .unwrap();

            cx_b.foreground().run_until_parked();
            let active_call_b = cx_b.read(ActiveCall::global);
            active_call_b
                .update(*cx_b, |call, cx| call.accept_incoming(cx))
                .await
                .unwrap();
        }
    }

    pub async fn build_app_state(
        test_db: &TestDb,
        fake_server: &live_kit_client::TestServer,
    ) -> Arc<AppState> {
        Arc::new(AppState {
            db: test_db.db().clone(),
            live_kit_client: Some(Arc::new(fake_server.create_api_client())),
            config: Default::default(),
        })
    }
}

impl Deref for TestServer {
    type Target = Server;

    fn deref(&self) -> &Self::Target {
        &self.server
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.server.teardown();
        self.test_live_kit_server.teardown().unwrap();
    }
}

impl Deref for TestClient {
    type Target = Arc<Client>;

    fn deref(&self) -> &Self::Target {
        &self.app_state.client
    }
}

impl TestClient {
    pub fn fs(&self) -> &FakeFs {
        self.app_state.fs.as_fake()
    }

    pub fn channel_store(&self) -> &ModelHandle<ChannelStore> {
        &self.channel_store
    }

    pub fn notification_store(&self) -> &ModelHandle<NotificationStore> {
        &self.notification_store
    }

    pub fn user_store(&self) -> &ModelHandle<UserStore> {
        &self.app_state.user_store
    }

    pub fn language_registry(&self) -> &Arc<LanguageRegistry> {
        &self.app_state.languages
    }

    pub fn client(&self) -> &Arc<Client> {
        &self.app_state.client
    }

    pub fn current_user_id(&self, cx: &TestAppContext) -> UserId {
        UserId::from_proto(
            self.app_state
                .user_store
                .read_with(cx, |user_store, _| user_store.current_user().unwrap().id),
        )
    }

    pub async fn wait_for_current_user(&self, cx: &TestAppContext) {
        let mut authed_user = self
            .app_state
            .user_store
            .read_with(cx, |user_store, _| user_store.watch_current_user());
        while authed_user.next().await.unwrap().is_none() {}
    }

    pub async fn clear_contacts(&self, cx: &mut TestAppContext) {
        self.app_state
            .user_store
            .update(cx, |store, _| store.clear_contacts())
            .await;
    }

    pub fn local_projects<'a>(&'a self) -> impl Deref<Target = Vec<ModelHandle<Project>>> + 'a {
        Ref::map(self.state.borrow(), |state| &state.local_projects)
    }

    pub fn remote_projects<'a>(&'a self) -> impl Deref<Target = Vec<ModelHandle<Project>>> + 'a {
        Ref::map(self.state.borrow(), |state| &state.remote_projects)
    }

    pub fn local_projects_mut<'a>(
        &'a self,
    ) -> impl DerefMut<Target = Vec<ModelHandle<Project>>> + 'a {
        RefMut::map(self.state.borrow_mut(), |state| &mut state.local_projects)
    }

    pub fn remote_projects_mut<'a>(
        &'a self,
    ) -> impl DerefMut<Target = Vec<ModelHandle<Project>>> + 'a {
        RefMut::map(self.state.borrow_mut(), |state| &mut state.remote_projects)
    }

    pub fn buffers_for_project<'a>(
        &'a self,
        project: &ModelHandle<Project>,
    ) -> impl DerefMut<Target = HashSet<ModelHandle<language::Buffer>>> + 'a {
        RefMut::map(self.state.borrow_mut(), |state| {
            state.buffers.entry(project.clone()).or_default()
        })
    }

    pub fn buffers<'a>(
        &'a self,
    ) -> impl DerefMut<Target = HashMap<ModelHandle<Project>, HashSet<ModelHandle<language::Buffer>>>> + 'a
    {
        RefMut::map(self.state.borrow_mut(), |state| &mut state.buffers)
    }

    pub fn channel_buffers<'a>(
        &'a self,
    ) -> impl DerefMut<Target = HashSet<ModelHandle<ChannelBuffer>>> + 'a {
        RefMut::map(self.state.borrow_mut(), |state| &mut state.channel_buffers)
    }

    pub fn summarize_contacts(&self, cx: &TestAppContext) -> ContactsSummary {
        self.app_state
            .user_store
            .read_with(cx, |store, _| ContactsSummary {
                current: store
                    .contacts()
                    .iter()
                    .map(|contact| contact.user.github_login.clone())
                    .collect(),
                outgoing_requests: store
                    .outgoing_contact_requests()
                    .iter()
                    .map(|user| user.github_login.clone())
                    .collect(),
                incoming_requests: store
                    .incoming_contact_requests()
                    .iter()
                    .map(|user| user.github_login.clone())
                    .collect(),
            })
    }

    pub async fn build_local_project(
        &self,
        root_path: impl AsRef<Path>,
        cx: &mut TestAppContext,
    ) -> (ModelHandle<Project>, WorktreeId) {
        let project = self.build_empty_local_project(cx);
        let (worktree, _) = project
            .update(cx, |p, cx| {
                p.find_or_create_local_worktree(root_path, true, cx)
            })
            .await
            .unwrap();
        worktree
            .read_with(cx, |tree, _| tree.as_local().unwrap().scan_complete())
            .await;
        (project, worktree.read_with(cx, |tree, _| tree.id()))
    }

    pub fn build_empty_local_project(&self, cx: &mut TestAppContext) -> ModelHandle<Project> {
        cx.update(|cx| {
            Project::local(
                self.client().clone(),
                self.app_state.node_runtime.clone(),
                self.app_state.user_store.clone(),
                self.app_state.languages.clone(),
                self.app_state.fs.clone(),
                cx,
            )
        })
    }

    pub async fn build_remote_project(
        &self,
        host_project_id: u64,
        guest_cx: &mut TestAppContext,
    ) -> ModelHandle<Project> {
        let active_call = guest_cx.read(ActiveCall::global);
        let room = active_call.read_with(guest_cx, |call, _| call.room().unwrap().clone());
        room.update(guest_cx, |room, cx| {
            room.join_project(
                host_project_id,
                self.app_state.languages.clone(),
                self.app_state.fs.clone(),
                cx,
            )
        })
        .await
        .unwrap()
    }

    pub fn build_workspace(
        &self,
        project: &ModelHandle<Project>,
        cx: &mut TestAppContext,
    ) -> WindowHandle<Workspace> {
        cx.add_window(|cx| Workspace::new(0, project.clone(), self.app_state.clone(), cx))
    }
}

impl Drop for TestClient {
    fn drop(&mut self) {
        self.app_state.client.teardown();
    }
}
