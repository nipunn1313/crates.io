use std::collections::HashMap;
use std::path::{Path, PathBuf};

use swirl::PerformError;
use tempfile::TempDir;
use url::Url;

use crate::models::DependencyKind;

static DEFAULT_GIT_SSH_USERNAME: &str = "git";

#[derive(Clone)]
pub enum Credentials {
    Missing,
    Http { username: String, password: String },
    Ssh { key: String },
}

impl Credentials {
    fn git2_callback(
        &self,
        user_from_url: Option<&str>,
        cred_type: git2::CredentialType,
    ) -> Result<git2::Cred, git2::Error> {
        match self {
            Credentials::Missing => Err(git2::Error::from_str("no authentication set")),
            Credentials::Http { username, password } => {
                git2::Cred::userpass_plaintext(username, password)
            }
            Credentials::Ssh { key } => {
                // git2 might call the callback two times when requesting credentials:
                //
                // 1. If the username is not specified in the URL, the first call will request it,
                //    without asking for the SSH key.
                //
                // 2. The other call will request the proper SSH key, and the username must be the
                //    same one either specified in the URL or the previous call.
                //
                // More information on this behavior is available at the following links:
                // - https://github.com/rust-lang/git2-rs/issues/329
                // - https://libgit2.org/docs/guides/authentication/
                let user = user_from_url.unwrap_or(DEFAULT_GIT_SSH_USERNAME);
                if cred_type.contains(git2::CredentialType::USERNAME) {
                    git2::Cred::username(user)
                } else {
                    git2::Cred::ssh_key_from_memory(user, None, key, None)
                }
            }
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Crate {
    pub name: String,
    pub vers: String,
    pub deps: Vec<Dependency>,
    pub cksum: String,
    pub features: HashMap<String, Vec<String>>,
    pub yanked: Option<bool>,
    #[serde(default)]
    pub links: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Dependency {
    pub name: String,
    pub req: String,
    pub features: Vec<String>,
    pub optional: bool,
    pub default_features: bool,
    pub target: Option<String>,
    pub kind: Option<DependencyKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
}

pub struct RepositoryConfig {
    pub index_location: Url,
    pub credentials: Credentials,
}

impl RepositoryConfig {
    pub fn from_environment() -> Self {
        let username = dotenv::var("GIT_HTTP_USER");
        let password = dotenv::var("GIT_HTTP_PWD");
        let http_url = dotenv::var("GIT_REPO_URL");

        let ssh_key = dotenv::var("GIT_SSH_KEY");
        let ssh_url = dotenv::var("GIT_SSH_REPO_URL");

        match (username, password, http_url, ssh_key, ssh_url) {
            (extra_user, extra_pass, extra_http_url, Ok(encoded_key), Ok(ssh_url)) => {
                if let (Ok(_), Ok(_), Ok(_)) = (extra_user, extra_pass, extra_http_url) {
                    println!(
                        "warning: both http and ssh credentials to authenticate with git are set"
                    );
                    println!("note: ssh credentials will take precedence over the http ones");
                }

                let index_location =
                    Url::parse(&ssh_url).expect("failed to parse GIT_SSH_REPO_URL");

                let credentials = Credentials::Ssh {
                    key: String::from_utf8(
                        base64::decode(&encoded_key).expect("failed to base64 decode the ssh key"),
                    )
                    .expect("failed to convert the ssh key to a string"),
                };

                Self {
                    index_location,
                    credentials,
                }
            }
            (Ok(username), Ok(password), Ok(http_url), Err(_), Err(_)) => {
                let index_location = Url::parse(&http_url).expect("failed to parse GIT_REPO_URL");
                let credentials = Credentials::Http { username, password };

                Self {
                    index_location,
                    credentials,
                }
            }
            (_, _, Ok(http_url), _, _) => {
                let index_location = Url::parse(&http_url).expect("failed to parse GIT_REPO_URL");
                let credentials = Credentials::Missing;

                Self {
                    index_location,
                    credentials,
                }
            }
            _ => panic!("must have `GIT_REPO_URL` defined"),
        }
    }
}

pub struct Repository {
    pub checkout_path: TempDir,
    repository: git2::Repository,
    pub credentials: Credentials,
}

impl Repository {
    pub fn open(repository_config: &RepositoryConfig) -> Result<Self, PerformError> {
        let checkout_path = tempfile::Builder::new().prefix("git").tempdir()?;

        let repository = git2::build::RepoBuilder::new()
            .fetch_options(Self::fetch_options(&repository_config.credentials))
            .remote_create(|repo, name, url| {
                // Manually create the remote with a fetchspec, to avoid cloning old snaphots
                repo.remote_with_fetch(
                    name,
                    url,
                    &format!("+refs/heads/master:refs/remotes/{}/master", name),
                )
            })
            .clone(
                repository_config.index_location.as_str(),
                checkout_path.path(),
            )?;

        // All commits to the index registry made through crates.io will be made by bors, the Rust
        // community's friendly GitHub bot.
        let mut cfg = repository.config()?;
        cfg.set_str("user.name", "bors")?;
        cfg.set_str("user.email", "bors@rust-lang.org")?;

        Ok(Self {
            checkout_path,
            repository,
            credentials: repository_config.credentials.clone(),
        })
    }

    pub fn index_file(&self, name: &str) -> PathBuf {
        self.checkout_path
            .path()
            .join(self.relative_index_file(name))
    }

    pub fn relative_index_file(&self, name: &str) -> PathBuf {
        let name = name.to_lowercase();
        match name.len() {
            1 => Path::new("1").join(&name),
            2 => Path::new("2").join(&name),
            3 => Path::new("3").join(&name[..1]).join(&name),
            _ => Path::new(&name[0..2]).join(&name[2..4]).join(&name),
        }
    }

    pub fn head_oid(&self) -> Result<git2::Oid, PerformError> {
        Ok(self.repository.head()?.target().unwrap())
    }

    fn perform_commit_and_push(&self, msg: &str, modified_file: &Path) -> Result<(), PerformError> {
        // git add $file
        let mut index = self.repository.index()?;
        index.add_path(modified_file)?;
        index.write()?;
        let tree_id = index.write_tree()?;
        let tree = self.repository.find_tree(tree_id)?;

        // git commit -m "..."
        let head = self.head_oid()?;
        let parent = self.repository.find_commit(head)?;
        let sig = self.repository.signature()?;
        self.repository
            .commit(Some("HEAD"), &sig, &sig, msg, &tree, &[&parent])?;

        self.push("refs/heads/master")
    }

    /// Push the current branch to the provided refname
    fn push(&self, refspec: &str) -> Result<(), PerformError> {
        let mut ref_status = Ok(());
        let mut callback_called = false;
        {
            let mut origin = self.repository.find_remote("origin")?;
            let mut callbacks = git2::RemoteCallbacks::new();
            callbacks.credentials(|_, user_from_url, cred_type| {
                self.credentials.git2_callback(user_from_url, cred_type)
            });
            callbacks.push_update_reference(|_, status| {
                if let Some(s) = status {
                    ref_status = Err(format!("failed to push a ref: {}", s).into())
                }
                callback_called = true;
                Ok(())
            });
            let mut opts = git2::PushOptions::new();
            opts.remote_callbacks(callbacks);
            origin.push(&[refspec], Some(&mut opts))?;
        }

        if !callback_called {
            ref_status = Err("update_reference callback was not called".into());
        }

        ref_status
    }

    pub fn commit_and_push(&self, message: &str, modified_file: &Path) -> Result<(), PerformError> {
        println!("Committing and pushing \"{}\"", message);

        self.perform_commit_and_push(message, modified_file)
            .map(|_| println!("Commit and push finished for \"{}\"", message))
            .map_err(|err| {
                eprintln!("Commit and push for \"{}\" errored: {}", message, err);
                err
            })
    }

    pub fn reset_head(&self) -> Result<(), PerformError> {
        let mut origin = self.repository.find_remote("origin")?;
        let original_head = self.head_oid()?;
        origin.fetch(
            // Force overwrite (`+` prefix) local master branch with the server's master branch.
            // The git CLI will refuse to fetch into the current branch of a non-bare repo
            // but libgit2 doesn't seem to prevent this potential footgun.
            // The entire point is to do a hard reset, so this footgun is not a concern.
            &["+refs/heads/master:refs/heads/master"],
            Some(&mut Self::fetch_options(&self.credentials)),
            None,
        )?;
        let head = self.head_oid()?;

        if head != original_head {
            println!("Resetting index from {} to {}", original_head, head);
        }

        let obj = self.repository.find_object(head, None)?;
        self.repository.reset(&obj, git2::ResetType::Hard, None)?;
        Ok(())
    }

    fn fetch_options(credentials: &Credentials) -> git2::FetchOptions<'_> {
        let mut callbacks = git2::RemoteCallbacks::new();
        callbacks.credentials(move |_, user_from_url, cred_type| {
            credentials.git2_callback(user_from_url, cred_type)
        });
        let mut opts = git2::FetchOptions::new();
        opts.remote_callbacks(callbacks);
        opts
    }

    /// Reset `HEAD` to a single commit with all the index contents, but no parent
    pub fn squash_to_single_commit(&self, msg: &str) -> Result<(), PerformError> {
        let tree = self.repository.find_commit(self.head_oid()?)?.tree()?;
        let sig = self.repository.signature()?;

        // We cannot update an existing `update_ref`, because that requires the
        // first parent of this commit to match the ref's current value.
        // Instead, create the commit and then do a hard reset.
        let commit = self.repository.commit(None, &sig, &sig, msg, &tree, &[])?;
        let commit = self
            .repository
            .find_object(commit, Some(git2::ObjectType::Commit))?;
        self.repository
            .reset(&commit, git2::ResetType::Hard, None)?;

        Ok(())
    }
}
