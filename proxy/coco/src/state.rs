//! Utility to work with the peer api of librad.

use std::{convert::TryFrom as _, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use librad::{
    git::{
        include::{self, Include},
        local::{transport, url::LocalUrl},
        refs::Refs,
        repo, storage,
        types::{namespace, NamespacedRef, Single},
    },
    git_ext::{OneLevel, RefLike},
    keys,
    meta::{entity, project as librad_project, user},
    net::peer::PeerApi,
    paths,
    peer::PeerId,
    uri::{RadUrl, RadUrn},
};
use radicle_keystore::sign::Signer as _;
use radicle_surf::vcs::{git, git::git2};

use crate::{
    peer::gossip,
    project::{self, peer},
    seed::Seed,
    signer, source,
    user::{verify as verify_user, User},
};

pub mod error;
pub use error::Error;

/// High-level interface to the coco monorepo and gossip layer.
#[derive(Clone)]
pub struct State {
    /// Internal handle on [`PeerApi`].
    pub(crate) api: PeerApi<keys::SecretKey>,
    /// Signer to sign artifacts generated by the user.
    signer: signer::BoxedSigner,
    /// A handle to the [`transport::Results`] which allows us to call [`transport::Results::wait`]
    /// on the results to ensure git has cleaned everything up.
    transport: transport::Settings,
}

impl State {
    /// Create a new [`State`] given a [`PeerApi`].
    #[must_use]
    pub fn new(api: PeerApi<keys::SecretKey>, signer: signer::BoxedSigner) -> Self {
        let paths = api.paths();

        // Register the transport so to use git2 to execute actions such as checkouts, fetch, and
        // push. The transport will then handle the interaction with the monorepo.
        transport::register();
        let transport = transport::Settings {
            paths: paths.clone(),
            signer: signer.clone(),
        };

        Self {
            api,
            signer,
            transport,
        }
    }

    /// Provide the caller with this state's [`transport::Results`] so that they can call
    /// [`transport::Results::wait`]. This should be used for testing purposes.
    ///
    /// See <https://github.com/radicle-dev/radicle-link/issues/381> for hope.
    #[must_use]
    pub fn transport_results(&self) -> Arc<transport::Results> {
        transport::LocalTransportFactory::configure(self.transport.clone())
    }

    /// Ensure that we give the local transport some time to process any final tasks. See
    /// [`transport::Results::wait`] for more information.
    fn process_transport_results(results: &Arc<transport::Results>) -> Result<(), Error> {
        if let Some(results) = results.wait(Duration::from_secs(3)) {
            for result in results {
                result.expect("transport thread panicked")?;
            }
        } else {
            log::warn!("While waiting for the transport results, we hit the timeout")
        }

        Ok(())
    }

    /// Returns the [`PathBuf`] to the underlying monorepo.
    #[must_use]
    pub fn monorepo(&self) -> PathBuf {
        self.api.paths().git_dir().join("")
    }

    /// Returns the underlying [`paths::Paths`].
    #[must_use]
    pub fn paths(&self) -> paths::Paths {
        self.api.paths().clone()
    }

    /// Check the storage to see if we have the given commit for project at `urn`.
    ///
    /// # Errors
    ///
    ///   * Checking the storage for the commit fails.
    pub async fn has_commit<Oid>(&self, urn: RadUrn, oid: Oid) -> Result<bool, Error>
    where
        Oid: Into<git2::Oid> + Send + 'static,
    {
        Ok(self
            .api
            .with_storage(move |storage| storage.has_commit(&urn, oid.into()))
            .await??)
    }

    /// The local machine's [`PeerId`].
    #[must_use]
    pub fn peer_id(&self) -> PeerId {
        self.api.peer_id()
    }

    /// The [`SocketAddr`] this [`PeerApi`] is listening on.
    #[must_use]
    pub fn listen_addr(&self) -> SocketAddr {
        self.api.listen_addr()
    }

    /// Get the default owner for this `PeerApi`.
    pub async fn default_owner(&self) -> Option<user::User<entity::Draft>> {
        self.api
            .with_storage(move |storage| {
                storage
                    .default_rad_self()
                    .map_err(|err| {
                        log::warn!("an error occurred while trying to get 'rad/self': {}", err)
                    })
                    .ok()
            })
            .await
            .ok()
            .flatten()
    }

    /// Set the default owner for this `PeerApi`.
    ///
    /// # Errors
    ///
    ///   * Fails to set the default `rad/self` for this `PeerApi`.
    pub async fn set_default_owner(&self, user: User) -> Result<(), Error> {
        self.api
            .with_storage(move |storage| storage.set_default_rad_self(user).map_err(Error::from))
            .await?
    }

    /// Initialise a [`User`] and make them the default owner of this [`PeerApi`].
    ///
    /// # Errors
    ///
    ///   * Fails to initialise `User`.
    ///   * Fails to verify `User`.
    ///   * Fails to set the default `rad/self` for this `PeerApi`.
    pub async fn init_owner(&self, handle: &str) -> Result<User, Error> {
        let user = self.init_user(handle).await?;
        let user = verify_user(user)?;

        self.set_default_owner(user.clone()).await?;

        Ok(user)
    }

    /// Given some hints as to where you might find it, get the urn of the project found at `url`.
    ///
    /// # Errors
    ///   * Could not successfully acquire a lock to the API.
    ///   * Could not open librad storage.
    ///   * Failed to clone the project.
    ///   * Failed to set the rad/self of this project.
    pub async fn clone_project<Addrs>(
        &self,
        url: RadUrl,
        addr_hints: Addrs,
    ) -> Result<RadUrn, Error>
    where
        Addrs: IntoIterator<Item = SocketAddr> + Send + 'static,
    {
        Ok(self
            .api
            .with_storage(move |storage| {
                let repo = storage.clone_repo::<librad_project::ProjectInfo, _>(url, addr_hints)?;
                repo.set_rad_self(storage::RadSelfSpec::Default)?;
                Ok::<_, repo::Error>(repo.urn)
            })
            .await??)
    }

    /// Get the project found at `urn`.
    ///
    /// # Errors
    ///
    ///   * Resolving the project fails.
    pub async fn get_project<P>(
        &self,
        urn: RadUrn,
        peer: P,
    ) -> Result<librad_project::Project<entity::Draft>, Error>
    where
        P: Into<Option<PeerId>> + Send + 'static,
    {
        Ok(self
            .api
            .with_storage(move |storage| storage.metadata_of(&urn, peer))
            .await??)
    }

    /// Returns the list of [`librad_project::Project`]s for the local peer.
    ///
    /// # Errors
    ///
    ///   * Retrieving the project entities from the store fails.
    #[allow(
        clippy::match_wildcard_for_single_variants,
        clippy::wildcard_enum_match_arm
    )]
    pub async fn list_projects(
        &self,
    ) -> Result<Vec<librad_project::Project<entity::Draft>>, Error> {
        let project_meta = self
            .api
            .with_storage(move |storage| {
                let owner = storage.default_rad_self()?;

                let meta = storage
                    .all_metadata()?
                    .flat_map(|entity| {
                        let entity = entity.ok()?;
                        let rad_self = storage.get_rad_self(&entity.urn()).ok()?;

                        // We only list projects that are owned by the peer
                        if rad_self.urn() != owner.urn() {
                            return None;
                        }

                        entity.try_map(|info| match info {
                            entity::data::EntityInfo::Project(info) => Some(info),
                            _ => None,
                        })
                    })
                    .collect::<Vec<_>>();

                Ok::<_, storage::Error>(meta)
            })
            .await??;

        Ok(project_meta)
    }

    /// Retrieves the [`librad::git::refs::Refs`] for the state owner.
    ///
    /// # Errors
    ///
    /// * if opening the storage fails
    pub async fn list_owner_project_refs(&self, urn: RadUrn) -> Result<Refs, Error> {
        Ok(self
            .api
            .with_storage(move |storage| storage.rad_signed_refs(&urn))
            .await??)
    }

    /// Retrieves the [`librad::git::refs::Refs`] for the given project urn.
    ///
    /// # Errors
    ///
    /// * if opening the storage fails
    pub async fn list_peer_project_refs(
        &self,
        urn: RadUrn,
        peer_id: PeerId,
    ) -> Result<Refs, Error> {
        Ok(self
            .api
            .with_storage(move |storage| storage.rad_signed_refs_of(&urn, peer_id))
            .await??)
    }

    /// Returns the list of [`user::User`]s known for your peer.
    ///
    /// # Errors
    ///
    ///   * Retrieval of the user entities from the store fails.
    #[allow(
        clippy::match_wildcard_for_single_variants,
        clippy::wildcard_enum_match_arm
    )]
    pub async fn list_users(&self) -> Result<Vec<user::User<entity::Draft>>, Error> {
        let entities = self
            .api
            .with_storage(move |storage| {
                let mut entities = vec![];
                for entity in storage.all_metadata()? {
                    let entity = entity?;

                    if let Some(e) = entity.try_map(|info| match info {
                        entity::data::EntityInfo::User(info) => Some(info),
                        _ => None,
                    }) {
                        entities.push(e);
                    }
                }

                Ok::<_, storage::Error>(entities)
            })
            .await??;

        Ok(entities)
    }

    /// Given some hints as to where you might find it, get the urn of the user found at `url`.
    ///
    /// # Errors
    ///
    ///   * Could not successfully acquire a lock to the API.
    ///   * Could not open librad storage.
    ///   * Failed to clone the user.
    pub async fn clone_user<Addrs>(&self, url: RadUrl, addr_hints: Addrs) -> Result<RadUrn, Error>
    where
        Addrs: IntoIterator<Item = SocketAddr> + Send + 'static,
    {
        Ok(self
            .api
            .with_storage(move |storage| {
                storage
                    .clone_repo::<user::UserInfo, _>(url, addr_hints)
                    .map(|repo| repo.urn)
            })
            .await??)
    }

    /// Get the user found at `urn`.
    ///
    /// # Errors
    ///
    ///   * Resolving the user fails.
    ///   * Could not successfully acquire a lock to the API.
    pub async fn get_user(&self, urn: RadUrn) -> Result<user::User<entity::Draft>, Error> {
        Ok(self
            .api
            .with_storage(move |storage| storage.metadata(&urn))
            .await??)
    }

    /// Fetch any updates at the given `RadUrl`, providing address hints if we have them.
    ///
    /// # Errors
    ///
    ///   * Could not successfully acquire a lock to the API.
    ///   * Could not open librad storage.
    ///   * Failed to fetch the updates.
    ///   * Failed to set the rad/self of this project.
    pub async fn fetch<Addrs>(&self, url: RadUrl, addr_hints: Addrs) -> Result<(), Error>
    where
        Addrs: IntoIterator<Item = SocketAddr> + Send + 'static,
    {
        Ok(self
            .api
            .with_storage(move |storage| storage.fetch_repo(url, addr_hints))
            .await??)
    }

    /// Provide a a repo [`git::Browser`] where the `Browser` is initialised with the provided
    /// `reference`.
    ///
    /// See [`State::find_default_branch`] and [`State::get_branch`] for obtaining a
    /// [`NamespacedRef`].
    ///
    /// # Errors
    ///   * If the namespace of the reference could not be converted to a [`git::Namespace`].
    ///   * If we could not open the backing storage.
    ///   * If we could not initialise the `Browser`.
    ///   * If the callback provided returned an error.
    pub async fn with_browser<F, T>(
        &self,
        reference: NamespacedRef<namespace::Legacy, Single>,
        callback: F,
    ) -> Result<T, Error>
    where
        F: FnOnce(&mut git::Browser) -> Result<T, source::Error> + Send,
    {
        let namespace = git::Namespace::try_from(reference.namespace().to_string().as_str())
            .map_err(source::Error::from)?;
        let branch = match reference.remote {
            None => git::Branch::local(reference.name.as_str()),
            Some(peer) => git::Branch::remote(
                &format!("heads/{}", reference.name.as_str()),
                &peer.to_string(),
            ),
        };
        let monorepo = self.monorepo();
        let repo = git::Repository::new(monorepo).map_err(source::Error::from)?;
        let mut browser = git::Browser::new_with_namespace(&repo, &namespace, branch)
            .map_err(source::Error::from)?;

        callback(&mut browser).map_err(Error::from)
    }

    /// This method helps us get a branch for a given [`RadUrn`] and optional [`PeerId`].
    ///
    /// If the `branch_name` is `None` then we get the project for the given [`RadUrn`] and use its
    /// `default_branch`.
    ///
    /// # Errors
    ///   * If the storage operations fail.
    ///   * If the requested reference was not found.
    pub async fn get_branch<P, B>(
        &self,
        urn: RadUrn,
        remote: P,
        branch_name: B,
    ) -> Result<NamespacedRef<namespace::Legacy, Single>, Error>
    where
        P: Into<Option<PeerId>> + Clone + Send,
        B: Into<Option<String>> + Clone + Send,
    {
        let name = match branch_name.into() {
            None => {
                let project = self.get_project(urn.clone(), None).await?;
                project.default_branch().to_owned()
            },
            Some(name) => name,
        }
        .parse()?;

        let remote = match remote.into() {
            Some(peer_id) if peer_id == self.peer_id() => None,
            Some(peer_id) => Some(peer_id),
            None => None,
        };
        let reference = NamespacedRef::head(urn.id, remote, name);
        let exists = {
            let reference = reference.clone();
            self.api
                .with_storage(move |storage| storage.has_ref(&reference))
                .await??
        };

        if exists {
            Ok(reference)
        } else {
            Err(Error::MissingRef { reference })
        }
    }

    /// This method helps us get the default branch for a given [`RadUrn`].
    ///
    /// It does this by:
    ///     * First checking if the owner of this storage has a reference to the default
    /// branch.
    ///     * If the owner does not have this reference then it falls back to the first maintainer.
    ///
    /// # Errors
    ///   * If the storage operations fail.
    ///   * If no default branch was found for the provided [`RadUrn`].
    pub async fn find_default_branch(
        &self,
        urn: RadUrn,
    ) -> Result<NamespacedRef<namespace::Legacy, Single>, Error> {
        let project = self.get_project(urn.clone(), None).await?;
        let peer = project.keys().iter().next().cloned().map(PeerId::from);
        let default_branch = project.default_branch();

        let (owner, peer) = tokio::join!(
            self.get_branch(urn.clone(), None, default_branch.to_owned()),
            self.get_branch(urn.clone(), peer, default_branch.to_owned())
        );
        match owner.or(peer) {
            Ok(reference) => Ok(reference),
            Err(Error::MissingRef { .. }) => Err(Error::NoDefaultBranch {
                name: project.name().to_string(),
                urn,
            }),
            Err(err) => Err(err),
        }
    }

    /// Initialize a [`librad_project::Project`] that is owned by the `owner`.
    /// This kicks off the history of the project, tracked by `librad`'s mono-repo.
    ///
    /// # Errors
    ///
    /// Will error if:
    ///     * The signing of the project metadata fails.
    ///     * The interaction with `librad` [`librad::git::storage::Storage`] fails.
    pub async fn init_project(
        &self,
        owner: &User,
        project: project::Create,
    ) -> Result<librad_project::Project<entity::Draft>, Error> {
        let mut meta = project.build(owner, self.signer.public_key().into())?;
        meta.sign_by_user(&self.signer, owner)?;

        let local_peer_id = self.api.peer_id();
        let url = LocalUrl::from_urn(meta.urn(), local_peer_id);

        let repository = project
            .validate(url)
            .map_err(project::create::Error::from)?;

        let meta = {
            let results = self.transport_results();
            let (meta, repo) = self
                .api
                .with_storage(move |storage| {
                    let _ = storage.create_repo(&meta)?;
                    log::debug!("Created project '{}#{}'", meta.urn(), meta.name());

                    let repo = repository
                        .setup_repo(meta.description().as_ref().unwrap_or(&String::default()))
                        .map_err(project::create::Error::from)?;

                    Ok::<_, Error>((meta, repo))
                })
                .await??;
            Self::process_transport_results(&results)?;
            let include_path = self.update_include(meta.urn()).await?;
            include::set_include_path(&repo, include_path)?;
            meta
        };

        crate::peer::gossip::announce(self, &meta.urn(), None).await;

        Ok(meta)
    }

    /// Create a [`user::User`] with the provided `handle`. This assumes that you are creating a
    /// user that uses the secret key the `PeerApi` was configured with.
    ///
    /// # Errors
    ///
    /// Will error if:
    ///     * The signing of the user metadata fails.
    ///     * The interaction with `librad` [`librad::git::storage::Storage`] fails.
    pub async fn init_user(&self, handle: &str) -> Result<user::User<entity::Draft>, Error> {
        let mut user = user::User::<entity::Draft>::create(
            handle.to_string(),
            self.signer.public_key().into(),
        )?;
        user.sign_owned(&self.signer)?;

        let user = self
            .api
            .with_storage(move |storage| {
                let _ = storage.create_repo(&user)?;
                Ok::<_, Error>(user)
            })
            .await??;

        Ok(user)
    }

    /// Wrapper around the storage track.
    ///
    /// # Errors
    ///
    /// * When the storage operation fails.
    pub async fn track(&self, urn: RadUrn, remote: PeerId) -> Result<(), Error> {
        {
            let urn = urn.clone();
            self.api
                .with_storage(move |storage| storage.track(&urn, &remote))
                .await??;
        }
        gossip::query(self, urn.clone(), Some(remote)).await;
        let path = self.update_include(urn).await?;
        log::debug!("Updated include path @ `{}`", path.display());
        Ok(())
    }

    /// Wrapper around the storage untrack.
    ///
    /// # Errors
    ///
    /// * When the storage operation fails.
    pub async fn untrack(&self, urn: RadUrn, remote: PeerId) -> Result<bool, Error> {
        let res = {
            let urn = urn.clone();
            self.api
                .with_storage(move |storage| storage.untrack(&urn, &remote))
                .await??
        };

        // Only need to update if we did untrack an existing peer
        if res {
            let path = self.update_include(urn).await?;
            log::debug!("Updated include path @ `{}`", path.display());
        }
        Ok(res)
    }

    /// Get the [`user::User`]s that are tracking this project, including their [`PeerId`].
    ///
    /// # Errors
    ///
    /// * If we could not acquire the lock
    /// * If we could not open the storage
    /// * If did not have the `urn` in storage
    /// * If we could not fetch the tracked peers
    /// * If we could not get the `rad/self` of the peer
    pub async fn tracked(
        &self,
        urn: RadUrn,
    ) -> Result<Vec<project::Peer<peer::Status<user::User<entity::Draft>>>>, Error> {
        let project = self.get_project(urn.clone(), None).await?;
        Ok(self
            .api
            .with_storage(move |storage| {
                let mut peers = vec![];
                let repo = storage.open_repo(urn)?;
                for peer_id in repo.tracked()? {
                    let status = if storage
                        .has_ref(&NamespacedRef::rad_self(repo.urn.id.clone(), peer_id))?
                    {
                        let user = repo.get_rad_self_of(peer_id)?;
                        if project.maintainers().contains(&user.urn()) {
                            peer::Status::replicated(peer::Role::Maintainer, user)
                        } else {
                            peer::Status::replicated(peer::Role::Contributor, user)
                        }
                    } else {
                        peer::Status::NotReplicated
                    };
                    peers.push(project::Peer::Remote { peer_id, status })
                }
                Ok::<_, Error>(peers)
            })
            .await??)
    }

    // TODO(xla): Account for projects not replicated but wanted.
    /// Constructs the list of [`project::Peer`] for the given `urn`. The basis is the list of
    /// tracking peers of the project combined with the local view.
    ///
    /// # Errors
    ///
    /// * if the project is not present in the monorepo
    /// * if the retrieval of tracking peers fails
    ///
    /// # Panics
    ///
    /// * if the default owner can't be fetched
    pub async fn list_project_peers(
        &self,
        urn: RadUrn,
    ) -> Result<Vec<project::Peer<peer::Status<user::User<entity::Draft>>>>, Error> {
        let project = self.get_project(urn.clone(), None).await?;

        let mut peers = vec![];

        let owner = self
            .default_owner()
            .await
            .expect("unable to find state owner");
        let refs = self.list_owner_project_refs(urn.clone()).await?;
        let status = if refs.heads.is_empty() {
            peer::Status::replicated(peer::Role::Tracker, owner)
        } else if project.maintainers().contains(&owner.urn()) {
            peer::Status::replicated(peer::Role::Maintainer, owner)
        } else {
            peer::Status::replicated(peer::Role::Contributor, owner)
        };

        peers.push(project::Peer::Local {
            peer_id: self.peer_id(),
            status,
        });

        let mut remotes = self.tracked(urn).await?;

        peers.append(&mut remotes);

        Ok(peers)
    }

    /// Creates a working copy for the project of the given `urn`.
    ///
    /// The `destination` is the directory where the caller wishes to place the working copy.
    ///
    /// The `peer_id` is from which peer we wish to base our checkout from.
    ///
    /// # Errors
    ///
    /// * if the project can't be found
    /// * if the include file creation fails
    /// * if the clone of the working copy fails
    pub async fn checkout<P>(
        &self,
        urn: RadUrn,
        peer_id: P,
        destination: PathBuf,
    ) -> Result<PathBuf, Error>
    where
        P: Into<Option<PeerId>> + Send + 'static,
    {
        let peer_id = peer_id.into();
        let proj = self.get_project(urn.clone(), peer_id).await?;
        let include_path = self.update_include(urn.clone()).await?;
        let default_branch: OneLevel = OneLevel::from(proj.default_branch().parse::<RefLike>()?);
        let checkout = project::Checkout {
            urn: proj.urn(),
            name: proj.name().to_string(),
            default_branch,
            path: destination,
            include_path,
        };

        let ownership = match peer_id {
            None => project::checkout::Ownership::Local(self.peer_id()),
            Some(remote) => {
                let handle = {
                    self.api
                        .with_storage(move |storage| {
                            let rad_self = storage.get_rad_self_of(&urn, remote)?;
                            Ok::<_, Error>(rad_self.name().to_string())
                        })
                        .await??
                };
                project::checkout::Ownership::Remote {
                    handle,
                    remote,
                    local: self.peer_id(),
                }
            },
        };

        let path = {
            let results = self.transport_results();
            let path =
                tokio::task::spawn_blocking(move || checkout.run(ownership).map_err(Error::from))
                    .await
                    .expect("blocking checkout failed")?;

            Self::process_transport_results(&results)?;
            path
        };

        Ok(path)
    }

    /// Prepare the include file for the given `project` with the latest tracked peers.
    ///
    /// # Errors
    ///
    /// * if getting the list of tracked peers fails
    pub async fn update_include(&self, urn: RadUrn) -> Result<PathBuf, Error> {
        let local_url = LocalUrl::from_urn(urn.clone(), self.peer_id());
        let tracked = self.tracked(urn).await?;
        let include = Include::from_tracked_users(
            self.paths().git_includes_dir().to_path_buf(),
            local_url,
            tracked
                .into_iter()
                .filter_map(|peer| project::Peer::replicated_remote(peer).map(|(p, u)| (u, p))),
        )?;
        let include_path = include.file_path();
        log::info!("creating include file @ '{:?}'", include_path);
        include.save()?;

        Ok(include_path)
    }
}

impl From<&State> for Seed {
    fn from(state: &State) -> Self {
        Self {
            peer_id: state.peer_id(),
            addr: state.listen_addr(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::panic)]
mod test {
    use std::{env, path::PathBuf};

    use librad::{git::storage, git_ext::OneLevel, keys::SecretKey, reflike};

    use crate::{config, control, project, signer};

    use super::{Error, State};

    fn fakie_project(path: PathBuf) -> project::Create {
        project::Create {
            repo: project::Repo::New {
                path,
                name: "fakie-nose-kickflip-backside-180-to-handplant".to_string(),
            },
            description: "rad git tricks".to_string(),
            default_branch: OneLevel::from(reflike!("dope")),
        }
    }

    fn radicle_project(path: PathBuf) -> project::Create {
        project::Create {
            repo: project::Repo::New {
                path,
                name: "radicalise".to_string(),
            },
            description: "the people".to_string(),
            default_branch: OneLevel::from(reflike!("power")),
        }
    }

    #[tokio::test]
    async fn can_create_user() -> Result<(), Box<dyn std::error::Error>> {
        let tmp_dir = tempfile::tempdir().expect("failed to create temdir");
        let key = SecretKey::new();
        let signer = signer::BoxedSigner::from(key);
        let config = config::default(key, tmp_dir.path())?;
        let (api, _run_loop) = config.try_into_peer().await?.accept()?;
        let state = State::new(api, signer);

        let annie = state.init_user("annie_are_you_ok?").await;
        assert!(annie.is_ok());

        Ok(())
    }

    #[tokio::test]
    async fn can_create_project() -> Result<(), Box<dyn std::error::Error>> {
        let tmp_dir = tempfile::tempdir().expect("failed to create temdir");
        env::set_var("RAD_HOME", tmp_dir.path());
        let repo_path = tmp_dir.path().join("radicle");
        let key = SecretKey::new();
        let signer = signer::BoxedSigner::from(key);
        let config = config::default(key, tmp_dir.path())?;
        let (api, _run_loop) = config.try_into_peer().await?.accept()?;
        let state = State::new(api, signer);

        let user = state.init_owner("cloudhead").await?;
        let project = state
            .init_project(&user, radicle_project(repo_path.clone()))
            .await;

        assert!(project.is_ok());
        assert!(repo_path.join("radicalise").exists());

        Ok(())
    }

    #[tokio::test]
    async fn can_create_project_for_existing_repo() -> Result<(), Box<dyn std::error::Error>> {
        let tmp_dir = tempfile::tempdir().expect("failed to create temdir");
        let repo_path = tmp_dir.path().join("radicle");
        let repo_path = repo_path.join("radicalise");
        let key = SecretKey::new();
        let signer = signer::BoxedSigner::from(key);
        let config = config::default(key, tmp_dir.path())?;
        let (api, _run_loop) = config.try_into_peer().await?.accept()?;
        let state = State::new(api, signer);

        let user = state.init_owner("cloudhead").await?;
        let project = state
            .init_project(&user, radicle_project(repo_path.clone()))
            .await;

        assert!(project.is_ok());
        assert!(repo_path.exists());

        Ok(())
    }

    #[tokio::test]
    async fn cannot_create_user_twice() -> Result<(), Box<dyn std::error::Error>> {
        let tmp_dir = tempfile::tempdir().expect("failed to create temdir");
        let key = SecretKey::new();
        let signer = signer::BoxedSigner::from(key);
        let config = config::default(key, tmp_dir.path())?;
        let (api, _run_loop) = config.try_into_peer().await?.accept()?;
        let state = State::new(api, signer);

        let user = state.init_owner("cloudhead").await?;
        let err = state.init_user("cloudhead").await;

        if let Err(Error::Storage(storage::Error::AlreadyExists(urn))) = err {
            assert_eq!(urn, user.urn())
        } else {
            panic!(
                "unexpected error when creating the user a second time: {:?}",
                err
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn cannot_create_project_twice() -> Result<(), Box<dyn std::error::Error>> {
        let tmp_dir = tempfile::tempdir().expect("failed to create temdir");
        let repo_path = tmp_dir.path().join("radicle");
        let key = SecretKey::new();
        let signer = signer::BoxedSigner::from(key);
        let config = config::default(key, tmp_dir.path())?;
        let (api, _run_loop) = config.try_into_peer().await?.accept()?;
        let state = State::new(api, signer);

        let user = state.init_owner("cloudhead").await?;
        let project_creation = radicle_project(repo_path.clone());
        let project = state.init_project(&user, project_creation.clone()).await?;

        let err = state
            .init_project(&user, project_creation.into_existing())
            .await;

        if let Err(Error::Storage(storage::Error::AlreadyExists(urn))) = err {
            assert_eq!(urn, project.urn())
        } else {
            panic!(
                "unexpected error when creating the project a second time: {:?}",
                err
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn list_projects() -> Result<(), Box<dyn std::error::Error>> {
        let tmp_dir = tempfile::tempdir().expect("failed to create temdir");
        let repo_path = tmp_dir.path().join("radicle");

        let key = SecretKey::new();
        let signer = signer::BoxedSigner::from(key);
        let config = config::default(key, tmp_dir.path())?;
        let (api, _run_loop) = config.try_into_peer().await?.accept()?;
        let state = State::new(api, signer);

        let user = state.init_owner("cloudhead").await?;

        control::setup_fixtures(&state, &user)
            .await
            .expect("unable to setup fixtures");

        let kalt = state.init_user("kalt").await?;
        let kalt = super::verify_user(kalt)?;
        let fakie = state.init_project(&kalt, fakie_project(repo_path)).await?;

        let projects = state.list_projects().await?;
        let mut project_names = projects
            .into_iter()
            .map(|project| project.name().to_string())
            .collect::<Vec<_>>();
        project_names.sort();

        assert_eq!(
            project_names,
            vec!["Monadic", "monokel", "open source coin", "radicle"]
        );

        assert!(!project_names.contains(&fakie.name().to_string()));

        Ok(())
    }

    #[tokio::test]
    async fn list_users() -> Result<(), Box<dyn std::error::Error>> {
        let tmp_dir = tempfile::tempdir().expect("failed to create temdir");
        let key = SecretKey::new();
        let signer = signer::BoxedSigner::from(key);
        let config = config::default(key, tmp_dir.path())?;
        let (api, _run_loop) = config.try_into_peer().await?.accept()?;
        let state = State::new(api, signer);

        let cloudhead = state.init_user("cloudhead").await?;
        let _cloudhead = super::verify_user(cloudhead)?;
        let kalt = state.init_user("kalt").await?;
        let _kalt = super::verify_user(kalt)?;

        let users = state.list_users().await?;
        let mut user_handles = users
            .into_iter()
            .map(|user| user.name().to_string())
            .collect::<Vec<_>>();
        user_handles.sort();

        assert_eq!(user_handles, vec!["cloudhead", "kalt"],);

        Ok(())
    }
}
