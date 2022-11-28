use crate::{Error, Result};
use anyhow::anyhow;
use axum::http::StatusCode;
use collections::{BTreeMap, HashMap, HashSet};
use futures::{future::BoxFuture, FutureExt, StreamExt};
use rpc::{proto, ConnectionId};
use serde::{Deserialize, Serialize};
use sqlx::{
    migrate::{Migrate as _, Migration, MigrationSource},
    types::Uuid,
    FromRow,
};
use std::{future::Future, path::Path, time::Duration};
use time::{OffsetDateTime, PrimitiveDateTime};

#[cfg(test)]
pub type DefaultDb = Db<sqlx::Sqlite>;

#[cfg(not(test))]
pub type DefaultDb = Db<sqlx::Postgres>;

pub struct Db<D: sqlx::Database> {
    pool: sqlx::Pool<D>,
    #[cfg(test)]
    background: Option<std::sync::Arc<gpui::executor::Background>>,
    #[cfg(test)]
    runtime: Option<tokio::runtime::Runtime>,
}

pub trait BeginTransaction: Send + Sync {
    type Database: sqlx::Database;

    fn begin_transaction(&self) -> BoxFuture<Result<sqlx::Transaction<'static, Self::Database>>>;
}

// In Postgres, serializable transactions are opt-in
impl BeginTransaction for Db<sqlx::Postgres> {
    type Database = sqlx::Postgres;

    fn begin_transaction(&self) -> BoxFuture<Result<sqlx::Transaction<'static, sqlx::Postgres>>> {
        async move {
            let mut tx = self.pool.begin().await?;
            sqlx::Executor::execute(&mut tx, "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE;")
                .await?;
            Ok(tx)
        }
        .boxed()
    }
}

// In Sqlite, transactions are inherently serializable.
#[cfg(test)]
impl BeginTransaction for Db<sqlx::Sqlite> {
    type Database = sqlx::Sqlite;

    fn begin_transaction(&self) -> BoxFuture<Result<sqlx::Transaction<'static, sqlx::Sqlite>>> {
        async move { Ok(self.pool.begin().await?) }.boxed()
    }
}

pub trait RowsAffected {
    fn rows_affected(&self) -> u64;
}

#[cfg(test)]
impl RowsAffected for sqlx::sqlite::SqliteQueryResult {
    fn rows_affected(&self) -> u64 {
        self.rows_affected()
    }
}

impl RowsAffected for sqlx::postgres::PgQueryResult {
    fn rows_affected(&self) -> u64 {
        self.rows_affected()
    }
}

#[cfg(test)]
impl Db<sqlx::Sqlite> {
    pub async fn new(url: &str, max_connections: u32) -> Result<Self> {
        use std::str::FromStr as _;
        let options = sqlx::sqlite::SqliteConnectOptions::from_str(url)
            .unwrap()
            .create_if_missing(true)
            .shared_cache(true);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .min_connections(2)
            .max_connections(max_connections)
            .connect_with(options)
            .await?;
        Ok(Self {
            pool,
            background: None,
            runtime: None,
        })
    }

    pub async fn get_users_by_ids(&self, ids: Vec<UserId>) -> Result<Vec<User>> {
        self.transact(|tx| async {
            let mut tx = tx;
            let query = "
                SELECT users.*
                FROM users
                WHERE users.id IN (SELECT value from json_each($1))
            ";
            Ok(sqlx::query_as(query)
                .bind(&serde_json::json!(ids))
                .fetch_all(&mut tx)
                .await?)
        })
        .await
    }

    pub async fn get_user_metrics_id(&self, id: UserId) -> Result<String> {
        self.transact(|mut tx| async move {
            let query = "
                SELECT metrics_id
                FROM users
                WHERE id = $1
            ";
            Ok(sqlx::query_scalar(query)
                .bind(id)
                .fetch_one(&mut tx)
                .await?)
        })
        .await
    }

    pub async fn create_user(
        &self,
        email_address: &str,
        admin: bool,
        params: NewUserParams,
    ) -> Result<NewUserResult> {
        self.transact(|mut tx| async {
            let query = "
                INSERT INTO users (email_address, github_login, github_user_id, admin, metrics_id)
                VALUES ($1, $2, $3, $4, $5)
                ON CONFLICT (github_login) DO UPDATE SET github_login = excluded.github_login
                RETURNING id, metrics_id
            ";

            let (user_id, metrics_id): (UserId, String) = sqlx::query_as(query)
                .bind(email_address)
                .bind(&params.github_login)
                .bind(&params.github_user_id)
                .bind(admin)
                .bind(Uuid::new_v4().to_string())
                .fetch_one(&mut tx)
                .await?;
            tx.commit().await?;
            Ok(NewUserResult {
                user_id,
                metrics_id,
                signup_device_id: None,
                inviting_user_id: None,
            })
        })
        .await
    }

    pub async fn fuzzy_search_users(&self, _name_query: &str, _limit: u32) -> Result<Vec<User>> {
        unimplemented!()
    }

    pub async fn create_user_from_invite(
        &self,
        _invite: &Invite,
        _user: NewUserParams,
    ) -> Result<Option<NewUserResult>> {
        unimplemented!()
    }

    pub async fn create_signup(&self, _signup: Signup) -> Result<()> {
        unimplemented!()
    }

    pub async fn create_invite_from_code(
        &self,
        _code: &str,
        _email_address: &str,
        _device_id: Option<&str>,
    ) -> Result<Invite> {
        unimplemented!()
    }

    pub async fn record_sent_invites(&self, _invites: &[Invite]) -> Result<()> {
        unimplemented!()
    }
}

impl Db<sqlx::Postgres> {
    pub async fn new(url: &str, max_connections: u32) -> Result<Self> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(url)
            .await?;
        Ok(Self {
            pool,
            #[cfg(test)]
            background: None,
            #[cfg(test)]
            runtime: None,
        })
    }

    #[cfg(test)]
    pub fn teardown(&self, url: &str) {
        self.runtime.as_ref().unwrap().block_on(async {
            use util::ResultExt;
            let query = "
                SELECT pg_terminate_backend(pg_stat_activity.pid)
                FROM pg_stat_activity
                WHERE pg_stat_activity.datname = current_database() AND pid <> pg_backend_pid();
            ";
            sqlx::query(query).execute(&self.pool).await.log_err();
            self.pool.close().await;
            <sqlx::Sqlite as sqlx::migrate::MigrateDatabase>::drop_database(url)
                .await
                .log_err();
        })
    }

    pub async fn fuzzy_search_users(&self, name_query: &str, limit: u32) -> Result<Vec<User>> {
        self.transact(|tx| async {
            let mut tx = tx;
            let like_string = Self::fuzzy_like_string(name_query);
            let query = "
                SELECT users.*
                FROM users
                WHERE github_login ILIKE $1
                ORDER BY github_login <-> $2
                LIMIT $3
            ";
            Ok(sqlx::query_as(query)
                .bind(like_string)
                .bind(name_query)
                .bind(limit as i32)
                .fetch_all(&mut tx)
                .await?)
        })
        .await
    }

    pub async fn get_users_by_ids(&self, ids: Vec<UserId>) -> Result<Vec<User>> {
        let ids = ids.iter().map(|id| id.0).collect::<Vec<_>>();
        self.transact(|tx| async {
            let mut tx = tx;
            let query = "
                SELECT users.*
                FROM users
                WHERE users.id = ANY ($1)
            ";
            Ok(sqlx::query_as(query).bind(&ids).fetch_all(&mut tx).await?)
        })
        .await
    }

    pub async fn get_user_metrics_id(&self, id: UserId) -> Result<String> {
        self.transact(|mut tx| async move {
            let query = "
                SELECT metrics_id::text
                FROM users
                WHERE id = $1
            ";
            Ok(sqlx::query_scalar(query)
                .bind(id)
                .fetch_one(&mut tx)
                .await?)
        })
        .await
    }

    pub async fn create_user(
        &self,
        email_address: &str,
        admin: bool,
        params: NewUserParams,
    ) -> Result<NewUserResult> {
        self.transact(|mut tx| async {
            let query = "
                INSERT INTO users (email_address, github_login, github_user_id, admin)
                VALUES ($1, $2, $3, $4)
                ON CONFLICT (github_login) DO UPDATE SET github_login = excluded.github_login
                RETURNING id, metrics_id::text
            ";

            let (user_id, metrics_id): (UserId, String) = sqlx::query_as(query)
                .bind(email_address)
                .bind(&params.github_login)
                .bind(params.github_user_id)
                .bind(admin)
                .fetch_one(&mut tx)
                .await?;
            tx.commit().await?;

            Ok(NewUserResult {
                user_id,
                metrics_id,
                signup_device_id: None,
                inviting_user_id: None,
            })
        })
        .await
    }

    pub async fn create_user_from_invite(
        &self,
        invite: &Invite,
        user: NewUserParams,
    ) -> Result<Option<NewUserResult>> {
        self.transact(|mut tx| async {
            let (signup_id, existing_user_id, inviting_user_id, signup_device_id): (
                i32,
                Option<UserId>,
                Option<UserId>,
                Option<String>,
            ) = sqlx::query_as(
                "
                SELECT id, user_id, inviting_user_id, device_id
                FROM signups
                WHERE
                    email_address = $1 AND
                    email_confirmation_code = $2
                ",
            )
            .bind(&invite.email_address)
            .bind(&invite.email_confirmation_code)
            .fetch_optional(&mut tx)
            .await?
            .ok_or_else(|| Error::Http(StatusCode::NOT_FOUND, "no such invite".to_string()))?;

            if existing_user_id.is_some() {
                return Ok(None);
            }

            let (user_id, metrics_id): (UserId, String) = sqlx::query_as(
                "
                INSERT INTO users
                (email_address, github_login, github_user_id, admin, invite_count, invite_code)
                VALUES
                ($1, $2, $3, FALSE, $4, $5)
                ON CONFLICT (github_login) DO UPDATE SET
                    email_address = excluded.email_address,
                    github_user_id = excluded.github_user_id,
                    admin = excluded.admin
                RETURNING id, metrics_id::text
                ",
            )
            .bind(&invite.email_address)
            .bind(&user.github_login)
            .bind(&user.github_user_id)
            .bind(&user.invite_count)
            .bind(random_invite_code())
            .fetch_one(&mut tx)
            .await?;

            sqlx::query(
                "
                UPDATE signups
                SET user_id = $1
                WHERE id = $2
                ",
            )
            .bind(&user_id)
            .bind(&signup_id)
            .execute(&mut tx)
            .await?;

            if let Some(inviting_user_id) = inviting_user_id {
                let id: Option<UserId> = sqlx::query_scalar(
                    "
                    UPDATE users
                    SET invite_count = invite_count - 1
                    WHERE id = $1 AND invite_count > 0
                    RETURNING id
                    ",
                )
                .bind(&inviting_user_id)
                .fetch_optional(&mut tx)
                .await?;

                if id.is_none() {
                    Err(Error::Http(
                        StatusCode::UNAUTHORIZED,
                        "no invites remaining".to_string(),
                    ))?;
                }

                sqlx::query(
                    "
                    INSERT INTO contacts
                        (user_id_a, user_id_b, a_to_b, should_notify, accepted)
                    VALUES
                        ($1, $2, TRUE, TRUE, TRUE)
                    ON CONFLICT DO NOTHING
                    ",
                )
                .bind(inviting_user_id)
                .bind(user_id)
                .execute(&mut tx)
                .await?;
            }

            tx.commit().await?;
            Ok(Some(NewUserResult {
                user_id,
                metrics_id,
                inviting_user_id,
                signup_device_id,
            }))
        })
        .await
    }

    pub async fn create_signup(&self, signup: Signup) -> Result<()> {
        self.transact(|mut tx| async {
            sqlx::query(
                "
                INSERT INTO signups
                (
                    email_address,
                    email_confirmation_code,
                    email_confirmation_sent,
                    platform_linux,
                    platform_mac,
                    platform_windows,
                    platform_unknown,
                    editor_features,
                    programming_languages,
                    device_id
                )
                VALUES
                    ($1, $2, FALSE, $3, $4, $5, FALSE, $6, $7, $8)
                RETURNING id
                ",
            )
            .bind(&signup.email_address)
            .bind(&random_email_confirmation_code())
            .bind(&signup.platform_linux)
            .bind(&signup.platform_mac)
            .bind(&signup.platform_windows)
            .bind(&signup.editor_features)
            .bind(&signup.programming_languages)
            .bind(&signup.device_id)
            .execute(&mut tx)
            .await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    pub async fn create_invite_from_code(
        &self,
        code: &str,
        email_address: &str,
        device_id: Option<&str>,
    ) -> Result<Invite> {
        self.transact(|mut tx| async {
            let existing_user: Option<UserId> = sqlx::query_scalar(
                "
                SELECT id
                FROM users
                WHERE email_address = $1
                ",
            )
            .bind(email_address)
            .fetch_optional(&mut tx)
            .await?;
            if existing_user.is_some() {
                Err(anyhow!("email address is already in use"))?;
            }

            let row: Option<(UserId, i32)> = sqlx::query_as(
                "
                SELECT id, invite_count
                FROM users
                WHERE invite_code = $1
                ",
            )
            .bind(code)
            .fetch_optional(&mut tx)
            .await?;

            let (inviter_id, invite_count) = match row {
                Some(row) => row,
                None => Err(Error::Http(
                    StatusCode::NOT_FOUND,
                    "invite code not found".to_string(),
                ))?,
            };

            if invite_count == 0 {
                Err(Error::Http(
                    StatusCode::UNAUTHORIZED,
                    "no invites remaining".to_string(),
                ))?;
            }

            let email_confirmation_code: String = sqlx::query_scalar(
                "
                INSERT INTO signups
                (
                    email_address,
                    email_confirmation_code,
                    email_confirmation_sent,
                    inviting_user_id,
                    platform_linux,
                    platform_mac,
                    platform_windows,
                    platform_unknown,
                    device_id
                )
                VALUES
                    ($1, $2, FALSE, $3, FALSE, FALSE, FALSE, TRUE, $4)
                ON CONFLICT (email_address)
                DO UPDATE SET
                    inviting_user_id = excluded.inviting_user_id
                RETURNING email_confirmation_code
                ",
            )
            .bind(&email_address)
            .bind(&random_email_confirmation_code())
            .bind(&inviter_id)
            .bind(&device_id)
            .fetch_one(&mut tx)
            .await?;

            tx.commit().await?;

            Ok(Invite {
                email_address: email_address.into(),
                email_confirmation_code,
            })
        })
        .await
    }

    pub async fn record_sent_invites(&self, invites: &[Invite]) -> Result<()> {
        self.transact(|mut tx| async {
            let emails = invites
                .iter()
                .map(|s| s.email_address.as_str())
                .collect::<Vec<_>>();
            sqlx::query(
                "
                UPDATE signups
                SET email_confirmation_sent = TRUE
                WHERE email_address = ANY ($1)
                ",
            )
            .bind(&emails)
            .execute(&mut tx)
            .await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }
}

impl<D> Db<D>
where
    Self: BeginTransaction<Database = D>,
    D: sqlx::Database + sqlx::migrate::MigrateDatabase,
    D::Connection: sqlx::migrate::Migrate,
    for<'a> <D as sqlx::database::HasArguments<'a>>::Arguments: sqlx::IntoArguments<'a, D>,
    for<'a> &'a mut D::Connection: sqlx::Executor<'a, Database = D>,
    for<'a, 'b> &'b mut sqlx::Transaction<'a, D>: sqlx::Executor<'b, Database = D>,
    D::QueryResult: RowsAffected,
    String: sqlx::Type<D>,
    i32: sqlx::Type<D>,
    i64: sqlx::Type<D>,
    bool: sqlx::Type<D>,
    str: sqlx::Type<D>,
    Uuid: sqlx::Type<D>,
    sqlx::types::Json<serde_json::Value>: sqlx::Type<D>,
    OffsetDateTime: sqlx::Type<D>,
    PrimitiveDateTime: sqlx::Type<D>,
    usize: sqlx::ColumnIndex<D::Row>,
    for<'a> &'a str: sqlx::ColumnIndex<D::Row>,
    for<'a> &'a str: sqlx::Encode<'a, D> + sqlx::Decode<'a, D>,
    for<'a> String: sqlx::Encode<'a, D> + sqlx::Decode<'a, D>,
    for<'a> Option<String>: sqlx::Encode<'a, D> + sqlx::Decode<'a, D>,
    for<'a> Option<&'a str>: sqlx::Encode<'a, D> + sqlx::Decode<'a, D>,
    for<'a> i32: sqlx::Encode<'a, D> + sqlx::Decode<'a, D>,
    for<'a> i64: sqlx::Encode<'a, D> + sqlx::Decode<'a, D>,
    for<'a> bool: sqlx::Encode<'a, D> + sqlx::Decode<'a, D>,
    for<'a> Uuid: sqlx::Encode<'a, D> + sqlx::Decode<'a, D>,
    for<'a> Option<ProjectId>: sqlx::Encode<'a, D> + sqlx::Decode<'a, D>,
    for<'a> sqlx::types::JsonValue: sqlx::Encode<'a, D> + sqlx::Decode<'a, D>,
    for<'a> OffsetDateTime: sqlx::Encode<'a, D> + sqlx::Decode<'a, D>,
    for<'a> PrimitiveDateTime: sqlx::Decode<'a, D> + sqlx::Decode<'a, D>,
{
    pub async fn migrate(
        &self,
        migrations_path: &Path,
        ignore_checksum_mismatch: bool,
    ) -> anyhow::Result<Vec<(Migration, Duration)>> {
        let migrations = MigrationSource::resolve(migrations_path)
            .await
            .map_err(|err| anyhow!("failed to load migrations: {err:?}"))?;

        let mut conn = self.pool.acquire().await?;

        conn.ensure_migrations_table().await?;
        let applied_migrations: HashMap<_, _> = conn
            .list_applied_migrations()
            .await?
            .into_iter()
            .map(|m| (m.version, m))
            .collect();

        let mut new_migrations = Vec::new();
        for migration in migrations {
            match applied_migrations.get(&migration.version) {
                Some(applied_migration) => {
                    if migration.checksum != applied_migration.checksum && !ignore_checksum_mismatch
                    {
                        Err(anyhow!(
                            "checksum mismatch for applied migration {}",
                            migration.description
                        ))?;
                    }
                }
                None => {
                    let elapsed = conn.apply(&migration).await?;
                    new_migrations.push((migration, elapsed));
                }
            }
        }

        Ok(new_migrations)
    }

    pub fn fuzzy_like_string(string: &str) -> String {
        let mut result = String::with_capacity(string.len() * 2 + 1);
        for c in string.chars() {
            if c.is_alphanumeric() {
                result.push('%');
                result.push(c);
            }
        }
        result.push('%');
        result
    }

    // users

    pub async fn get_all_users(&self, page: u32, limit: u32) -> Result<Vec<User>> {
        self.transact(|tx| async {
            let mut tx = tx;
            let query = "SELECT * FROM users ORDER BY github_login ASC LIMIT $1 OFFSET $2";
            Ok(sqlx::query_as(query)
                .bind(limit as i32)
                .bind((page * limit) as i32)
                .fetch_all(&mut tx)
                .await?)
        })
        .await
    }

    pub async fn get_user_by_id(&self, id: UserId) -> Result<Option<User>> {
        self.transact(|tx| async {
            let mut tx = tx;
            let query = "
                SELECT users.*
                FROM users
                WHERE id = $1
                LIMIT 1
            ";
            Ok(sqlx::query_as(query)
                .bind(&id)
                .fetch_optional(&mut tx)
                .await?)
        })
        .await
    }

    pub async fn get_users_with_no_invites(
        &self,
        invited_by_another_user: bool,
    ) -> Result<Vec<User>> {
        self.transact(|tx| async {
            let mut tx = tx;
            let query = format!(
                "
                SELECT users.*
                FROM users
                WHERE invite_count = 0
                AND inviter_id IS{} NULL
                ",
                if invited_by_another_user { " NOT" } else { "" }
            );

            Ok(sqlx::query_as(&query).fetch_all(&mut tx).await?)
        })
        .await
    }

    pub async fn get_user_by_github_account(
        &self,
        github_login: &str,
        github_user_id: Option<i32>,
    ) -> Result<Option<User>> {
        self.transact(|tx| async {
            let mut tx = tx;
            if let Some(github_user_id) = github_user_id {
                let mut user = sqlx::query_as::<_, User>(
                    "
                    UPDATE users
                    SET github_login = $1
                    WHERE github_user_id = $2
                    RETURNING *
                    ",
                )
                .bind(github_login)
                .bind(github_user_id)
                .fetch_optional(&mut tx)
                .await?;

                if user.is_none() {
                    user = sqlx::query_as::<_, User>(
                        "
                        UPDATE users
                        SET github_user_id = $1
                        WHERE github_login = $2
                        RETURNING *
                        ",
                    )
                    .bind(github_user_id)
                    .bind(github_login)
                    .fetch_optional(&mut tx)
                    .await?;
                }

                Ok(user)
            } else {
                let user = sqlx::query_as(
                    "
                    SELECT * FROM users
                    WHERE github_login = $1
                    LIMIT 1
                    ",
                )
                .bind(github_login)
                .fetch_optional(&mut tx)
                .await?;
                Ok(user)
            }
        })
        .await
    }

    pub async fn set_user_is_admin(&self, id: UserId, is_admin: bool) -> Result<()> {
        self.transact(|mut tx| async {
            let query = "UPDATE users SET admin = $1 WHERE id = $2";
            sqlx::query(query)
                .bind(is_admin)
                .bind(id.0)
                .execute(&mut tx)
                .await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    pub async fn set_user_connected_once(&self, id: UserId, connected_once: bool) -> Result<()> {
        self.transact(|mut tx| async move {
            let query = "UPDATE users SET connected_once = $1 WHERE id = $2";
            sqlx::query(query)
                .bind(connected_once)
                .bind(id.0)
                .execute(&mut tx)
                .await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    pub async fn destroy_user(&self, id: UserId) -> Result<()> {
        self.transact(|mut tx| async move {
            let query = "DELETE FROM access_tokens WHERE user_id = $1;";
            sqlx::query(query)
                .bind(id.0)
                .execute(&mut tx)
                .await
                .map(drop)?;
            let query = "DELETE FROM users WHERE id = $1;";
            sqlx::query(query).bind(id.0).execute(&mut tx).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    // signups

    pub async fn get_waitlist_summary(&self) -> Result<WaitlistSummary> {
        self.transact(|mut tx| async move {
            Ok(sqlx::query_as(
                "
                SELECT
                    COUNT(*) as count,
                    COALESCE(SUM(CASE WHEN platform_linux THEN 1 ELSE 0 END), 0) as linux_count,
                    COALESCE(SUM(CASE WHEN platform_mac THEN 1 ELSE 0 END), 0) as mac_count,
                    COALESCE(SUM(CASE WHEN platform_windows THEN 1 ELSE 0 END), 0) as windows_count,
                    COALESCE(SUM(CASE WHEN platform_unknown THEN 1 ELSE 0 END), 0) as unknown_count
                FROM (
                    SELECT *
                    FROM signups
                    WHERE
                        NOT email_confirmation_sent
                ) AS unsent
                ",
            )
            .fetch_one(&mut tx)
            .await?)
        })
        .await
    }

    pub async fn get_unsent_invites(&self, count: usize) -> Result<Vec<Invite>> {
        self.transact(|mut tx| async move {
            Ok(sqlx::query_as(
                "
                SELECT
                    email_address, email_confirmation_code
                FROM signups
                WHERE
                    NOT email_confirmation_sent AND
                    (platform_mac OR platform_unknown)
                LIMIT $1
                ",
            )
            .bind(count as i32)
            .fetch_all(&mut tx)
            .await?)
        })
        .await
    }

    // invite codes

    pub async fn set_invite_count_for_user(&self, id: UserId, count: u32) -> Result<()> {
        self.transact(|mut tx| async move {
            if count > 0 {
                sqlx::query(
                    "
                    UPDATE users
                    SET invite_code = $1
                    WHERE id = $2 AND invite_code IS NULL
                ",
                )
                .bind(random_invite_code())
                .bind(id)
                .execute(&mut tx)
                .await?;
            }

            sqlx::query(
                "
                UPDATE users
                SET invite_count = $1
                WHERE id = $2
                ",
            )
            .bind(count as i32)
            .bind(id)
            .execute(&mut tx)
            .await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    pub async fn get_invite_code_for_user(&self, id: UserId) -> Result<Option<(String, u32)>> {
        self.transact(|mut tx| async move {
            let result: Option<(String, i32)> = sqlx::query_as(
                "
                    SELECT invite_code, invite_count
                    FROM users
                    WHERE id = $1 AND invite_code IS NOT NULL 
                ",
            )
            .bind(id)
            .fetch_optional(&mut tx)
            .await?;
            if let Some((code, count)) = result {
                Ok(Some((code, count.try_into().map_err(anyhow::Error::new)?)))
            } else {
                Ok(None)
            }
        })
        .await
    }

    pub async fn get_user_for_invite_code(&self, code: &str) -> Result<User> {
        self.transact(|tx| async {
            let mut tx = tx;
            sqlx::query_as(
                "
                    SELECT *
                    FROM users
                    WHERE invite_code = $1
                ",
            )
            .bind(code)
            .fetch_optional(&mut tx)
            .await?
            .ok_or_else(|| {
                Error::Http(
                    StatusCode::NOT_FOUND,
                    "that invite code does not exist".to_string(),
                )
            })
        })
        .await
    }

    pub async fn create_room(
        &self,
        user_id: UserId,
        connection_id: ConnectionId,
    ) -> Result<proto::Room> {
        self.transact(|mut tx| async move {
            let live_kit_room = nanoid::nanoid!(30);
            let room_id = sqlx::query_scalar(
                "
                INSERT INTO rooms (live_kit_room)
                VALUES ($1)
                RETURNING id
                ",
            )
            .bind(&live_kit_room)
            .fetch_one(&mut tx)
            .await
            .map(RoomId)?;

            sqlx::query(
                "
                INSERT INTO room_participants (room_id, user_id, answering_connection_id, calling_user_id, calling_connection_id)
                VALUES ($1, $2, $3, $4, $5)
                ",
            )
            .bind(room_id)
            .bind(user_id)
            .bind(connection_id.0 as i32)
            .bind(user_id)
            .bind(connection_id.0 as i32)
            .execute(&mut tx)
            .await?;

            let room = self.get_room(room_id, &mut tx).await?;
            tx.commit().await?;
            Ok(room)
        }).await
    }

    pub async fn call(
        &self,
        room_id: RoomId,
        calling_user_id: UserId,
        calling_connection_id: ConnectionId,
        called_user_id: UserId,
        initial_project_id: Option<ProjectId>,
    ) -> Result<(proto::Room, proto::IncomingCall)> {
        self.transact(|mut tx| async move {
            sqlx::query(
                "
                INSERT INTO room_participants (room_id, user_id, calling_user_id, calling_connection_id, initial_project_id)
                VALUES ($1, $2, $3, $4, $5)
                ",
            )
            .bind(room_id)
            .bind(called_user_id)
            .bind(calling_user_id)
            .bind(calling_connection_id.0 as i32)
            .bind(initial_project_id)
            .execute(&mut tx)
            .await?;

            let room = self.get_room(room_id, &mut tx).await?;
                tx.commit().await?;

            let incoming_call = Self::build_incoming_call(&room, called_user_id)
                .ok_or_else(|| anyhow!("failed to build incoming call"))?;
            Ok((room, incoming_call))
        }).await
    }

    pub async fn incoming_call_for_user(
        &self,
        user_id: UserId,
    ) -> Result<Option<proto::IncomingCall>> {
        self.transact(|mut tx| async move {
            let room_id = sqlx::query_scalar::<_, RoomId>(
                "
                SELECT room_id
                FROM room_participants
                WHERE user_id = $1 AND answering_connection_id IS NULL
                ",
            )
            .bind(user_id)
            .fetch_optional(&mut tx)
            .await?;

            if let Some(room_id) = room_id {
                let room = self.get_room(room_id, &mut tx).await?;
                Ok(Self::build_incoming_call(&room, user_id))
            } else {
                Ok(None)
            }
        })
        .await
    }

    fn build_incoming_call(
        room: &proto::Room,
        called_user_id: UserId,
    ) -> Option<proto::IncomingCall> {
        let pending_participant = room
            .pending_participants
            .iter()
            .find(|participant| participant.user_id == called_user_id.to_proto())?;

        Some(proto::IncomingCall {
            room_id: room.id,
            calling_user_id: pending_participant.calling_user_id,
            participant_user_ids: room
                .participants
                .iter()
                .map(|participant| participant.user_id)
                .collect(),
            initial_project: room.participants.iter().find_map(|participant| {
                let initial_project_id = pending_participant.initial_project_id?;
                participant
                    .projects
                    .iter()
                    .find(|project| project.id == initial_project_id)
                    .cloned()
            }),
        })
    }

    pub async fn call_failed(
        &self,
        room_id: RoomId,
        called_user_id: UserId,
    ) -> Result<proto::Room> {
        self.transact(|mut tx| async move {
            sqlx::query(
                "
                DELETE FROM room_participants
                WHERE room_id = $1 AND user_id = $2
                ",
            )
            .bind(room_id)
            .bind(called_user_id)
            .execute(&mut tx)
            .await?;

            let room = self.get_room(room_id, &mut tx).await?;
            tx.commit().await?;
            Ok(room)
        })
        .await
    }

    pub async fn decline_call(
        &self,
        expected_room_id: Option<RoomId>,
        user_id: UserId,
    ) -> Result<proto::Room> {
        self.transact(|mut tx| async move {
            let room_id = sqlx::query_scalar(
                "
                DELETE FROM room_participants
                WHERE user_id = $1 AND answering_connection_id IS NULL
                RETURNING room_id
                ",
            )
            .bind(user_id)
            .fetch_one(&mut tx)
            .await?;
            if expected_room_id.map_or(false, |expected_room_id| expected_room_id != room_id) {
                return Err(anyhow!("declining call on unexpected room"))?;
            }

            let room = self.get_room(room_id, &mut tx).await?;
            tx.commit().await?;
            Ok(room)
        })
        .await
    }

    pub async fn cancel_call(
        &self,
        expected_room_id: Option<RoomId>,
        calling_connection_id: ConnectionId,
        called_user_id: UserId,
    ) -> Result<proto::Room> {
        self.transact(|mut tx| async move {
            let room_id = sqlx::query_scalar(
                "
                DELETE FROM room_participants
                WHERE user_id = $1 AND calling_connection_id = $2 AND answering_connection_id IS NULL
                RETURNING room_id
                ",
            )
            .bind(called_user_id)
            .bind(calling_connection_id.0 as i32)
            .fetch_one(&mut tx)
            .await?;
            if expected_room_id.map_or(false, |expected_room_id| expected_room_id != room_id) {
                return Err(anyhow!("canceling call on unexpected room"))?;
            }

            let room = self.get_room(room_id, &mut tx).await?;
            tx.commit().await?;
            Ok(room)
        }).await
    }

    pub async fn join_room(
        &self,
        room_id: RoomId,
        user_id: UserId,
        connection_id: ConnectionId,
    ) -> Result<proto::Room> {
        self.transact(|mut tx| async move {
            sqlx::query(
                "
                UPDATE room_participants 
                SET answering_connection_id = $1
                WHERE room_id = $2 AND user_id = $3
                RETURNING 1
                ",
            )
            .bind(connection_id.0 as i32)
            .bind(room_id)
            .bind(user_id)
            .fetch_one(&mut tx)
            .await?;

            let room = self.get_room(room_id, &mut tx).await?;
            tx.commit().await?;
            Ok(room)
        })
        .await
    }

    pub async fn leave_room(&self, connection_id: ConnectionId) -> Result<Option<LeftRoom>> {
        self.transact(|mut tx| async move {
            // Leave room.
            let room_id = sqlx::query_scalar::<_, RoomId>(
                "
                DELETE FROM room_participants
                WHERE answering_connection_id = $1
                RETURNING room_id
                ",
            )
            .bind(connection_id.0 as i32)
            .fetch_optional(&mut tx)
            .await?;

            if let Some(room_id) = room_id {
                // Cancel pending calls initiated by the leaving user.
                let canceled_calls_to_user_ids: Vec<UserId> = sqlx::query_scalar(
                    "
                    DELETE FROM room_participants
                    WHERE calling_connection_id = $1 AND answering_connection_id IS NULL
                    RETURNING user_id
                    ",
                )
                .bind(connection_id.0 as i32)
                .fetch_all(&mut tx)
                .await?;

                let project_ids = sqlx::query_scalar::<_, ProjectId>(
                    "
                    SELECT project_id
                    FROM project_collaborators
                    WHERE connection_id = $1
                    ",
                )
                .bind(connection_id.0 as i32)
                .fetch_all(&mut tx)
                .await?;

                // Leave projects.
                let mut left_projects = HashMap::default();
                if !project_ids.is_empty() {
                    let mut params = "?,".repeat(project_ids.len());
                    params.pop();
                    let query = format!(
                        "
                        SELECT *
                        FROM project_collaborators
                        WHERE project_id IN ({params})
                    "
                    );
                    let mut query = sqlx::query_as::<_, ProjectCollaborator>(&query);
                    for project_id in project_ids {
                        query = query.bind(project_id);
                    }

                    let mut project_collaborators = query.fetch(&mut tx);
                    while let Some(collaborator) = project_collaborators.next().await {
                        let collaborator = collaborator?;
                        let left_project =
                            left_projects
                                .entry(collaborator.project_id)
                                .or_insert(LeftProject {
                                    id: collaborator.project_id,
                                    host_user_id: Default::default(),
                                    connection_ids: Default::default(),
                                    host_connection_id: Default::default(),
                                });

                        let collaborator_connection_id =
                            ConnectionId(collaborator.connection_id as u32);
                        if collaborator_connection_id != connection_id {
                            left_project.connection_ids.push(collaborator_connection_id);
                        }

                        if collaborator.is_host {
                            left_project.host_user_id = collaborator.user_id;
                            left_project.host_connection_id =
                                ConnectionId(collaborator.connection_id as u32);
                        }
                    }
                }
                sqlx::query(
                    "
                    DELETE FROM project_collaborators
                    WHERE connection_id = $1
                    ",
                )
                .bind(connection_id.0 as i32)
                .execute(&mut tx)
                .await?;

                // Unshare projects.
                sqlx::query(
                    "
                    DELETE FROM projects
                    WHERE room_id = $1 AND host_connection_id = $2
                    ",
                )
                .bind(room_id)
                .bind(connection_id.0 as i32)
                .execute(&mut tx)
                .await?;

                let room = self.get_room(room_id, &mut tx).await?;
                tx.commit().await?;

                Ok(Some(LeftRoom {
                    room,
                    left_projects,
                    canceled_calls_to_user_ids,
                }))
            } else {
                Ok(None)
            }
        })
        .await
    }

    pub async fn update_room_participant_location(
        &self,
        room_id: RoomId,
        connection_id: ConnectionId,
        location: proto::ParticipantLocation,
    ) -> Result<proto::Room> {
        self.transact(|tx| async {
            let mut tx = tx;
            let location_kind;
            let location_project_id;
            match location
                .variant
                .as_ref()
                .ok_or_else(|| anyhow!("invalid location"))?
            {
                proto::participant_location::Variant::SharedProject(project) => {
                    location_kind = 0;
                    location_project_id = Some(ProjectId::from_proto(project.id));
                }
                proto::participant_location::Variant::UnsharedProject(_) => {
                    location_kind = 1;
                    location_project_id = None;
                }
                proto::participant_location::Variant::External(_) => {
                    location_kind = 2;
                    location_project_id = None;
                }
            }

            sqlx::query(
                "
                UPDATE room_participants
                SET location_kind = $1, location_project_id = $2
                WHERE room_id = $3 AND answering_connection_id = $4
                RETURNING 1
                ",
            )
            .bind(location_kind)
            .bind(location_project_id)
            .bind(room_id)
            .bind(connection_id.0 as i32)
            .fetch_one(&mut tx)
            .await?;

            let room = self.get_room(room_id, &mut tx).await?;
            tx.commit().await?;
            Ok(room)
        })
        .await
    }

    async fn get_guest_connection_ids(
        &self,
        project_id: ProjectId,
        tx: &mut sqlx::Transaction<'_, D>,
    ) -> Result<Vec<ConnectionId>> {
        let mut guest_connection_ids = Vec::new();
        let mut db_guest_connection_ids = sqlx::query_scalar::<_, i32>(
            "
            SELECT connection_id
            FROM project_collaborators
            WHERE project_id = $1 AND is_host = FALSE
            ",
        )
        .bind(project_id)
        .fetch(tx);
        while let Some(connection_id) = db_guest_connection_ids.next().await {
            guest_connection_ids.push(ConnectionId(connection_id? as u32));
        }
        Ok(guest_connection_ids)
    }

    async fn get_room(
        &self,
        room_id: RoomId,
        tx: &mut sqlx::Transaction<'_, D>,
    ) -> Result<proto::Room> {
        let room: Room = sqlx::query_as(
            "
            SELECT *
            FROM rooms
            WHERE id = $1
            ",
        )
        .bind(room_id)
        .fetch_one(&mut *tx)
        .await?;

        let mut db_participants =
            sqlx::query_as::<_, (UserId, Option<i32>, Option<i32>, Option<ProjectId>, UserId, Option<ProjectId>)>(
                "
                SELECT user_id, answering_connection_id, location_kind, location_project_id, calling_user_id, initial_project_id
                FROM room_participants
                WHERE room_id = $1
                ",
            )
            .bind(room_id)
            .fetch(&mut *tx);

        let mut participants = HashMap::default();
        let mut pending_participants = Vec::new();
        while let Some(participant) = db_participants.next().await {
            let (
                user_id,
                answering_connection_id,
                location_kind,
                location_project_id,
                calling_user_id,
                initial_project_id,
            ) = participant?;
            if let Some(answering_connection_id) = answering_connection_id {
                let location = match (location_kind, location_project_id) {
                    (Some(0), Some(project_id)) => {
                        Some(proto::participant_location::Variant::SharedProject(
                            proto::participant_location::SharedProject {
                                id: project_id.to_proto(),
                            },
                        ))
                    }
                    (Some(1), _) => Some(proto::participant_location::Variant::UnsharedProject(
                        Default::default(),
                    )),
                    _ => Some(proto::participant_location::Variant::External(
                        Default::default(),
                    )),
                };
                participants.insert(
                    answering_connection_id,
                    proto::Participant {
                        user_id: user_id.to_proto(),
                        peer_id: answering_connection_id as u32,
                        projects: Default::default(),
                        location: Some(proto::ParticipantLocation { variant: location }),
                    },
                );
            } else {
                pending_participants.push(proto::PendingParticipant {
                    user_id: user_id.to_proto(),
                    calling_user_id: calling_user_id.to_proto(),
                    initial_project_id: initial_project_id.map(|id| id.to_proto()),
                });
            }
        }
        drop(db_participants);

        let mut rows = sqlx::query_as::<_, (i32, ProjectId, Option<String>)>(
            "
            SELECT host_connection_id, projects.id, worktrees.root_name
            FROM projects
            LEFT JOIN worktrees ON projects.id = worktrees.project_id
            WHERE room_id = $1
            ",
        )
        .bind(room_id)
        .fetch(&mut *tx);

        while let Some(row) = rows.next().await {
            let (connection_id, project_id, worktree_root_name) = row?;
            if let Some(participant) = participants.get_mut(&connection_id) {
                let project = if let Some(project) = participant
                    .projects
                    .iter_mut()
                    .find(|project| project.id == project_id.to_proto())
                {
                    project
                } else {
                    participant.projects.push(proto::ParticipantProject {
                        id: project_id.to_proto(),
                        worktree_root_names: Default::default(),
                    });
                    participant.projects.last_mut().unwrap()
                };
                project.worktree_root_names.extend(worktree_root_name);
            }
        }

        Ok(proto::Room {
            id: room.id.to_proto(),
            live_kit_room: room.live_kit_room,
            participants: participants.into_values().collect(),
            pending_participants,
        })
    }

    // projects

    pub async fn project_count_excluding_admins(&self) -> Result<usize> {
        self.transact(|mut tx| async move {
            Ok(sqlx::query_scalar::<_, i32>(
                "
                SELECT COUNT(*)
                FROM projects, users
                WHERE projects.host_user_id = users.id AND users.admin IS FALSE
                ",
            )
            .fetch_one(&mut tx)
            .await? as usize)
        })
        .await
    }

    pub async fn share_project(
        &self,
        expected_room_id: RoomId,
        connection_id: ConnectionId,
        worktrees: &[proto::WorktreeMetadata],
    ) -> Result<(ProjectId, proto::Room)> {
        self.transact(|mut tx| async move {
            let (room_id, user_id) = sqlx::query_as::<_, (RoomId, UserId)>(
                "
                SELECT room_id, user_id
                FROM room_participants
                WHERE answering_connection_id = $1
                ",
            )
            .bind(connection_id.0 as i32)
            .fetch_one(&mut tx)
            .await?;
            if room_id != expected_room_id {
                return Err(anyhow!("shared project on unexpected room"))?;
            }

            let project_id: ProjectId = sqlx::query_scalar(
                "
                INSERT INTO projects (room_id, host_user_id, host_connection_id)
                VALUES ($1, $2, $3)
                RETURNING id
                ",
            )
            .bind(room_id)
            .bind(user_id)
            .bind(connection_id.0 as i32)
            .fetch_one(&mut tx)
            .await?;

            if !worktrees.is_empty() {
                let mut params = "(?, ?, ?, ?, ?, ?, ?),".repeat(worktrees.len());
                params.pop();
                let query = format!(
                    "
                    INSERT INTO worktrees (
                        project_id,
                        id,
                        root_name,
                        abs_path,
                        visible,
                        scan_id,
                        is_complete
                    )
                    VALUES {params}
                    "
                );

                let mut query = sqlx::query(&query);
                for worktree in worktrees {
                    query = query
                        .bind(project_id)
                        .bind(worktree.id as i32)
                        .bind(&worktree.root_name)
                        .bind(&worktree.abs_path)
                        .bind(worktree.visible)
                        .bind(0)
                        .bind(false);
                }
                query.execute(&mut tx).await?;
            }

            sqlx::query(
                "
                INSERT INTO project_collaborators (
                    project_id,
                    connection_id,
                    user_id,
                    replica_id,
                    is_host
                )
                VALUES ($1, $2, $3, $4, $5)
                ",
            )
            .bind(project_id)
            .bind(connection_id.0 as i32)
            .bind(user_id)
            .bind(0)
            .bind(true)
            .execute(&mut tx)
            .await?;

            let room = self.get_room(room_id, &mut tx).await?;
            tx.commit().await?;

            Ok((project_id, room))
        })
        .await
    }

    pub async fn unshare_project(
        &self,
        project_id: ProjectId,
        connection_id: ConnectionId,
    ) -> Result<(proto::Room, Vec<ConnectionId>)> {
        self.transact(|mut tx| async move {
            let guest_connection_ids = self.get_guest_connection_ids(project_id, &mut tx).await?;
            let room_id: RoomId = sqlx::query_scalar(
                "
                DELETE FROM projects
                WHERE id = $1 AND host_connection_id = $2
                RETURNING room_id
                ",
            )
            .bind(project_id)
            .bind(connection_id.0 as i32)
            .fetch_one(&mut tx)
            .await?;
            let room = self.get_room(room_id, &mut tx).await?;
            tx.commit().await?;

            Ok((room, guest_connection_ids))
        })
        .await
    }

    pub async fn update_project(
        &self,
        project_id: ProjectId,
        connection_id: ConnectionId,
        worktrees: &[proto::WorktreeMetadata],
    ) -> Result<(proto::Room, Vec<ConnectionId>)> {
        self.transact(|mut tx| async move {
            let room_id: RoomId = sqlx::query_scalar(
                "
                SELECT room_id
                FROM projects
                WHERE id = $1 AND host_connection_id = $2
                ",
            )
            .bind(project_id)
            .bind(connection_id.0 as i32)
            .fetch_one(&mut tx)
            .await?;

            if !worktrees.is_empty() {
                let mut params = "(?, ?, ?, ?, ?, ?, ?),".repeat(worktrees.len());
                params.pop();
                let query = format!(
                    "
                    INSERT INTO worktrees (
                        project_id,
                        id,
                        root_name,
                        abs_path,
                        visible,
                        scan_id,
                        is_complete
                    )
                    VALUES {params}
                    ON CONFLICT (project_id, id) DO UPDATE SET root_name = excluded.root_name
                    "
                );

                let mut query = sqlx::query(&query);
                for worktree in worktrees {
                    query = query
                        .bind(project_id)
                        .bind(worktree.id as i32)
                        .bind(&worktree.root_name)
                        .bind(&worktree.abs_path)
                        .bind(worktree.visible)
                        .bind(0)
                        .bind(false)
                }
                query.execute(&mut tx).await?;
            }

            let mut params = "?,".repeat(worktrees.len());
            if !worktrees.is_empty() {
                params.pop();
            }
            let query = format!(
                "
                DELETE FROM worktrees
                WHERE project_id = ? AND id NOT IN ({params})
                ",
            );

            let mut query = sqlx::query(&query).bind(project_id);
            for worktree in worktrees {
                query = query.bind(WorktreeId(worktree.id as i32));
            }
            query.execute(&mut tx).await?;

            let guest_connection_ids = self.get_guest_connection_ids(project_id, &mut tx).await?;
            let room = self.get_room(room_id, &mut tx).await?;
            tx.commit().await?;

            Ok((room, guest_connection_ids))
        })
        .await
    }

    pub async fn update_worktree(
        &self,
        update: &proto::UpdateWorktree,
        connection_id: ConnectionId,
    ) -> Result<Vec<ConnectionId>> {
        self.transact(|mut tx| async move {
            let project_id = ProjectId::from_proto(update.project_id);
            let worktree_id = WorktreeId::from_proto(update.worktree_id);

            // Ensure the update comes from the host.
            sqlx::query(
                "
                SELECT 1
                FROM projects
                WHERE id = $1 AND host_connection_id = $2
                ",
            )
            .bind(project_id)
            .bind(connection_id.0 as i32)
            .fetch_one(&mut tx)
            .await?;

            // Update metadata.
            sqlx::query(
                "
                UPDATE worktrees
                SET
                    root_name = $1,
                    scan_id = $2,
                    is_complete = $3,
                    abs_path = $4
                WHERE project_id = $5 AND id = $6
                RETURNING 1
                ",
            )
            .bind(&update.root_name)
            .bind(update.scan_id as i64)
            .bind(update.is_last_update)
            .bind(&update.abs_path)
            .bind(project_id)
            .bind(worktree_id)
            .fetch_one(&mut tx)
            .await?;

            if !update.updated_entries.is_empty() {
                let mut params =
                    "(?, ?, ?, ?, ?, ?, ?, ?, ?, ?),".repeat(update.updated_entries.len());
                params.pop();

                let query = format!(
                    "
                    INSERT INTO worktree_entries (
                        project_id, 
                        worktree_id, 
                        id, 
                        is_dir, 
                        path, 
                        inode,
                        mtime_seconds, 
                        mtime_nanos, 
                        is_symlink, 
                        is_ignored
                    )
                    VALUES {params}
                    ON CONFLICT (project_id, worktree_id, id) DO UPDATE SET
                        is_dir = excluded.is_dir,
                        path = excluded.path,
                        inode = excluded.inode,
                        mtime_seconds = excluded.mtime_seconds,
                        mtime_nanos = excluded.mtime_nanos,
                        is_symlink = excluded.is_symlink,
                        is_ignored = excluded.is_ignored
                    "
                );
                let mut query = sqlx::query(&query);
                for entry in &update.updated_entries {
                    let mtime = entry.mtime.clone().unwrap_or_default();
                    query = query
                        .bind(project_id)
                        .bind(worktree_id)
                        .bind(entry.id as i64)
                        .bind(entry.is_dir)
                        .bind(&entry.path)
                        .bind(entry.inode as i64)
                        .bind(mtime.seconds as i64)
                        .bind(mtime.nanos as i32)
                        .bind(entry.is_symlink)
                        .bind(entry.is_ignored);
                }
                query.execute(&mut tx).await?;
            }

            if !update.removed_entries.is_empty() {
                let mut params = "?,".repeat(update.removed_entries.len());
                params.pop();
                let query = format!(
                    "
                    DELETE FROM worktree_entries
                    WHERE project_id = ? AND worktree_id = ? AND id IN ({params})
                    "
                );

                let mut query = sqlx::query(&query).bind(project_id).bind(worktree_id);
                for entry_id in &update.removed_entries {
                    query = query.bind(*entry_id as i64);
                }
                query.execute(&mut tx).await?;
            }

            let connection_ids = self.get_guest_connection_ids(project_id, &mut tx).await?;
            tx.commit().await?;
            Ok(connection_ids)
        })
        .await
    }

    pub async fn update_diagnostic_summary(
        &self,
        update: &proto::UpdateDiagnosticSummary,
        connection_id: ConnectionId,
    ) -> Result<Vec<ConnectionId>> {
        self.transact(|mut tx| async {
            let project_id = ProjectId::from_proto(update.project_id);
            let worktree_id = WorktreeId::from_proto(update.worktree_id);
            let summary = update
                .summary
                .as_ref()
                .ok_or_else(|| anyhow!("invalid summary"))?;

            // Ensure the update comes from the host.
            sqlx::query(
                "
                SELECT 1
                FROM projects
                WHERE id = $1 AND host_connection_id = $2
                ",
            )
            .bind(project_id)
            .bind(connection_id.0 as i32)
            .fetch_one(&mut tx)
            .await?;

            // Update summary.
            sqlx::query(
                "
                INSERT INTO worktree_diagnostic_summaries (
                    project_id,
                    worktree_id,
                    path,
                    language_server_id,
                    error_count,
                    warning_count
                )
                VALUES ($1, $2, $3, $4, $5, $6)
                ON CONFLICT (project_id, worktree_id, path) DO UPDATE SET
                    language_server_id = excluded.language_server_id,
                    error_count = excluded.error_count, 
                    warning_count = excluded.warning_count
                ",
            )
            .bind(project_id)
            .bind(worktree_id)
            .bind(&summary.path)
            .bind(summary.language_server_id as i64)
            .bind(summary.error_count as i32)
            .bind(summary.warning_count as i32)
            .execute(&mut tx)
            .await?;

            let connection_ids = self.get_guest_connection_ids(project_id, &mut tx).await?;
            tx.commit().await?;
            Ok(connection_ids)
        })
        .await
    }

    pub async fn start_language_server(
        &self,
        update: &proto::StartLanguageServer,
        connection_id: ConnectionId,
    ) -> Result<Vec<ConnectionId>> {
        self.transact(|mut tx| async {
            let project_id = ProjectId::from_proto(update.project_id);
            let server = update
                .server
                .as_ref()
                .ok_or_else(|| anyhow!("invalid language server"))?;

            // Ensure the update comes from the host.
            sqlx::query(
                "
                SELECT 1
                FROM projects
                WHERE id = $1 AND host_connection_id = $2
                ",
            )
            .bind(project_id)
            .bind(connection_id.0 as i32)
            .fetch_one(&mut tx)
            .await?;

            // Add the newly-started language server.
            sqlx::query(
                "
                INSERT INTO language_servers (project_id, id, name)
                VALUES ($1, $2, $3)
                ON CONFLICT (project_id, id) DO UPDATE SET
                    name = excluded.name
                ",
            )
            .bind(project_id)
            .bind(server.id as i64)
            .bind(&server.name)
            .execute(&mut tx)
            .await?;

            let connection_ids = self.get_guest_connection_ids(project_id, &mut tx).await?;
            tx.commit().await?;
            Ok(connection_ids)
        })
        .await
    }

    pub async fn join_project(
        &self,
        project_id: ProjectId,
        connection_id: ConnectionId,
    ) -> Result<(Project, ReplicaId)> {
        self.transact(|mut tx| async move {
            let (room_id, user_id) = sqlx::query_as::<_, (RoomId, UserId)>(
                "
                SELECT room_id, user_id
                FROM room_participants
                WHERE answering_connection_id = $1
                ",
            )
            .bind(connection_id.0 as i32)
            .fetch_one(&mut tx)
            .await?;

            // Ensure project id was shared on this room.
            sqlx::query(
                "
                SELECT 1
                FROM projects
                WHERE id = $1 AND room_id = $2
                ",
            )
            .bind(project_id)
            .bind(room_id)
            .fetch_one(&mut tx)
            .await?;

            let mut collaborators = sqlx::query_as::<_, ProjectCollaborator>(
                "
                SELECT *
                FROM project_collaborators
                WHERE project_id = $1
                ",
            )
            .bind(project_id)
            .fetch_all(&mut tx)
            .await?;
            let replica_ids = collaborators
                .iter()
                .map(|c| c.replica_id)
                .collect::<HashSet<_>>();
            let mut replica_id = ReplicaId(1);
            while replica_ids.contains(&replica_id) {
                replica_id.0 += 1;
            }
            let new_collaborator = ProjectCollaborator {
                project_id,
                connection_id: connection_id.0 as i32,
                user_id,
                replica_id,
                is_host: false,
            };

            sqlx::query(
                "
                INSERT INTO project_collaborators (
                    project_id,
                    connection_id,
                    user_id,
                    replica_id,
                    is_host
                )
                VALUES ($1, $2, $3, $4, $5)
                ",
            )
            .bind(new_collaborator.project_id)
            .bind(new_collaborator.connection_id)
            .bind(new_collaborator.user_id)
            .bind(new_collaborator.replica_id)
            .bind(new_collaborator.is_host)
            .execute(&mut tx)
            .await?;
            collaborators.push(new_collaborator);

            let worktree_rows = sqlx::query_as::<_, WorktreeRow>(
                "
                SELECT *
                FROM worktrees
                WHERE project_id = $1
                ",
            )
            .bind(project_id)
            .fetch_all(&mut tx)
            .await?;
            let mut worktrees = worktree_rows
                .into_iter()
                .map(|worktree_row| {
                    (
                        worktree_row.id,
                        Worktree {
                            id: worktree_row.id,
                            abs_path: worktree_row.abs_path,
                            root_name: worktree_row.root_name,
                            visible: worktree_row.visible,
                            entries: Default::default(),
                            diagnostic_summaries: Default::default(),
                            scan_id: worktree_row.scan_id as u64,
                            is_complete: worktree_row.is_complete,
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>();

            // Populate worktree entries.
            {
                let mut entries = sqlx::query_as::<_, WorktreeEntry>(
                    "
                    SELECT *
                    FROM worktree_entries
                    WHERE project_id = $1
                    ",
                )
                .bind(project_id)
                .fetch(&mut tx);
                while let Some(entry) = entries.next().await {
                    let entry = entry?;
                    if let Some(worktree) = worktrees.get_mut(&entry.worktree_id) {
                        worktree.entries.push(proto::Entry {
                            id: entry.id as u64,
                            is_dir: entry.is_dir,
                            path: entry.path,
                            inode: entry.inode as u64,
                            mtime: Some(proto::Timestamp {
                                seconds: entry.mtime_seconds as u64,
                                nanos: entry.mtime_nanos as u32,
                            }),
                            is_symlink: entry.is_symlink,
                            is_ignored: entry.is_ignored,
                        });
                    }
                }
            }

            // Populate worktree diagnostic summaries.
            {
                let mut summaries = sqlx::query_as::<_, WorktreeDiagnosticSummary>(
                    "
                    SELECT *
                    FROM worktree_diagnostic_summaries
                    WHERE project_id = $1
                    ",
                )
                .bind(project_id)
                .fetch(&mut tx);
                while let Some(summary) = summaries.next().await {
                    let summary = summary?;
                    if let Some(worktree) = worktrees.get_mut(&summary.worktree_id) {
                        worktree
                            .diagnostic_summaries
                            .push(proto::DiagnosticSummary {
                                path: summary.path,
                                language_server_id: summary.language_server_id as u64,
                                error_count: summary.error_count as u32,
                                warning_count: summary.warning_count as u32,
                            });
                    }
                }
            }

            // Populate language servers.
            let language_servers = sqlx::query_as::<_, LanguageServer>(
                "
                SELECT *
                FROM language_servers
                WHERE project_id = $1
                ",
            )
            .bind(project_id)
            .fetch_all(&mut tx)
            .await?;

            tx.commit().await?;
            Ok((
                Project {
                    collaborators,
                    worktrees,
                    language_servers: language_servers
                        .into_iter()
                        .map(|language_server| proto::LanguageServer {
                            id: language_server.id.to_proto(),
                            name: language_server.name,
                        })
                        .collect(),
                },
                replica_id as ReplicaId,
            ))
        })
        .await
    }

    pub async fn leave_project(
        &self,
        project_id: ProjectId,
        connection_id: ConnectionId,
    ) -> Result<LeftProject> {
        self.transact(|mut tx| async move {
            let result = sqlx::query(
                "
                DELETE FROM project_collaborators
                WHERE project_id = $1 AND connection_id = $2
                ",
            )
            .bind(project_id)
            .bind(connection_id.0 as i32)
            .execute(&mut tx)
            .await?;

            if result.rows_affected() == 0 {
                Err(anyhow!("not a collaborator on this project"))?;
            }

            let connection_ids = sqlx::query_scalar::<_, i32>(
                "
                SELECT connection_id
                FROM project_collaborators
                WHERE project_id = $1
                ",
            )
            .bind(project_id)
            .fetch_all(&mut tx)
            .await?
            .into_iter()
            .map(|id| ConnectionId(id as u32))
            .collect();

            let (host_user_id, host_connection_id) = sqlx::query_as::<_, (i32, i32)>(
                "
                SELECT host_user_id, host_connection_id
                FROM projects
                WHERE id = $1
                ",
            )
            .bind(project_id)
            .fetch_one(&mut tx)
            .await?;

            tx.commit().await?;

            Ok(LeftProject {
                id: project_id,
                host_user_id: UserId(host_user_id),
                host_connection_id: ConnectionId(host_connection_id as u32),
                connection_ids,
            })
        })
        .await
    }

    pub async fn project_collaborators(
        &self,
        project_id: ProjectId,
        connection_id: ConnectionId,
    ) -> Result<Vec<ProjectCollaborator>> {
        self.transact(|mut tx| async move {
            let collaborators = sqlx::query_as::<_, ProjectCollaborator>(
                "
                SELECT *
                FROM project_collaborators
                WHERE project_id = $1
                ",
            )
            .bind(project_id)
            .fetch_all(&mut tx)
            .await?;

            if collaborators
                .iter()
                .any(|collaborator| collaborator.connection_id == connection_id.0 as i32)
            {
                Ok(collaborators)
            } else {
                Err(anyhow!("no such project"))?
            }
        })
        .await
    }

    pub async fn project_connection_ids(
        &self,
        project_id: ProjectId,
        connection_id: ConnectionId,
    ) -> Result<HashSet<ConnectionId>> {
        self.transact(|mut tx| async move {
            let connection_ids = sqlx::query_scalar::<_, i32>(
                "
                SELECT connection_id
                FROM project_collaborators
                WHERE project_id = $1
                ",
            )
            .bind(project_id)
            .fetch_all(&mut tx)
            .await?;

            if connection_ids.contains(&(connection_id.0 as i32)) {
                Ok(connection_ids
                    .into_iter()
                    .map(|connection_id| ConnectionId(connection_id as u32))
                    .collect())
            } else {
                Err(anyhow!("no such project"))?
            }
        })
        .await
    }

    // contacts

    pub async fn get_contacts(&self, user_id: UserId) -> Result<Vec<Contact>> {
        self.transact(|mut tx| async move {
            let query = "
                SELECT user_id_a, user_id_b, a_to_b, accepted, should_notify, (room_participants.id IS NOT NULL) as busy
                FROM contacts
                LEFT JOIN room_participants ON room_participants.user_id = $1
                WHERE user_id_a = $1 OR user_id_b = $1;
            ";

            let mut rows = sqlx::query_as::<_, (UserId, UserId, bool, bool, bool, bool)>(query)
                .bind(user_id)
                .fetch(&mut tx);

            let mut contacts = Vec::new();
            while let Some(row) = rows.next().await {
                let (user_id_a, user_id_b, a_to_b, accepted, should_notify, busy) = row?;
                if user_id_a == user_id {
                    if accepted {
                        contacts.push(Contact::Accepted {
                            user_id: user_id_b,
                            should_notify: should_notify && a_to_b,
                            busy
                        });
                    } else if a_to_b {
                        contacts.push(Contact::Outgoing { user_id: user_id_b })
                    } else {
                        contacts.push(Contact::Incoming {
                            user_id: user_id_b,
                            should_notify,
                        });
                    }
                } else if accepted {
                    contacts.push(Contact::Accepted {
                        user_id: user_id_a,
                        should_notify: should_notify && !a_to_b,
                        busy
                    });
                } else if a_to_b {
                    contacts.push(Contact::Incoming {
                        user_id: user_id_a,
                        should_notify,
                    });
                } else {
                    contacts.push(Contact::Outgoing { user_id: user_id_a });
                }
            }

            contacts.sort_unstable_by_key(|contact| contact.user_id());

            Ok(contacts)
        })
        .await
    }

    pub async fn is_user_busy(&self, user_id: UserId) -> Result<bool> {
        self.transact(|mut tx| async move {
            Ok(sqlx::query_scalar::<_, i32>(
                "
                SELECT 1
                FROM room_participants
                WHERE room_participants.user_id = $1
                ",
            )
            .bind(user_id)
            .fetch_optional(&mut tx)
            .await?
            .is_some())
        })
        .await
    }

    pub async fn has_contact(&self, user_id_1: UserId, user_id_2: UserId) -> Result<bool> {
        self.transact(|mut tx| async move {
            let (id_a, id_b) = if user_id_1 < user_id_2 {
                (user_id_1, user_id_2)
            } else {
                (user_id_2, user_id_1)
            };

            let query = "
                SELECT 1 FROM contacts
                WHERE user_id_a = $1 AND user_id_b = $2 AND accepted = TRUE
                LIMIT 1
            ";
            Ok(sqlx::query_scalar::<_, i32>(query)
                .bind(id_a.0)
                .bind(id_b.0)
                .fetch_optional(&mut tx)
                .await?
                .is_some())
        })
        .await
    }

    pub async fn send_contact_request(&self, sender_id: UserId, receiver_id: UserId) -> Result<()> {
        self.transact(|mut tx| async move {
            let (id_a, id_b, a_to_b) = if sender_id < receiver_id {
                (sender_id, receiver_id, true)
            } else {
                (receiver_id, sender_id, false)
            };
            let query = "
                INSERT into contacts (user_id_a, user_id_b, a_to_b, accepted, should_notify)
                VALUES ($1, $2, $3, FALSE, TRUE)
                ON CONFLICT (user_id_a, user_id_b) DO UPDATE
                SET
                    accepted = TRUE,
                    should_notify = FALSE
                WHERE
                    NOT contacts.accepted AND
                    ((contacts.a_to_b = excluded.a_to_b AND contacts.user_id_a = excluded.user_id_b) OR
                    (contacts.a_to_b != excluded.a_to_b AND contacts.user_id_a = excluded.user_id_a));
            ";
            let result = sqlx::query(query)
                .bind(id_a.0)
                .bind(id_b.0)
                .bind(a_to_b)
                .execute(&mut tx)
                .await?;

            if result.rows_affected() == 1 {
                tx.commit().await?;
                Ok(())
            } else {
                Err(anyhow!("contact already requested"))?
            }
        }).await
    }

    pub async fn remove_contact(&self, requester_id: UserId, responder_id: UserId) -> Result<()> {
        self.transact(|mut tx| async move {
            let (id_a, id_b) = if responder_id < requester_id {
                (responder_id, requester_id)
            } else {
                (requester_id, responder_id)
            };
            let query = "
                DELETE FROM contacts
                WHERE user_id_a = $1 AND user_id_b = $2;
            ";
            let result = sqlx::query(query)
                .bind(id_a.0)
                .bind(id_b.0)
                .execute(&mut tx)
                .await?;

            if result.rows_affected() == 1 {
                tx.commit().await?;
                Ok(())
            } else {
                Err(anyhow!("no such contact"))?
            }
        })
        .await
    }

    pub async fn dismiss_contact_notification(
        &self,
        user_id: UserId,
        contact_user_id: UserId,
    ) -> Result<()> {
        self.transact(|mut tx| async move {
            let (id_a, id_b, a_to_b) = if user_id < contact_user_id {
                (user_id, contact_user_id, true)
            } else {
                (contact_user_id, user_id, false)
            };

            let query = "
                UPDATE contacts
                SET should_notify = FALSE
                WHERE
                    user_id_a = $1 AND user_id_b = $2 AND
                    (
                        (a_to_b = $3 AND accepted) OR
                        (a_to_b != $3 AND NOT accepted)
                    );
            ";

            let result = sqlx::query(query)
                .bind(id_a.0)
                .bind(id_b.0)
                .bind(a_to_b)
                .execute(&mut tx)
                .await?;

            if result.rows_affected() == 0 {
                Err(anyhow!("no such contact request"))?
            } else {
                tx.commit().await?;
                Ok(())
            }
        })
        .await
    }

    pub async fn respond_to_contact_request(
        &self,
        responder_id: UserId,
        requester_id: UserId,
        accept: bool,
    ) -> Result<()> {
        self.transact(|mut tx| async move {
            let (id_a, id_b, a_to_b) = if responder_id < requester_id {
                (responder_id, requester_id, false)
            } else {
                (requester_id, responder_id, true)
            };
            let result = if accept {
                let query = "
                    UPDATE contacts
                    SET accepted = TRUE, should_notify = TRUE
                    WHERE user_id_a = $1 AND user_id_b = $2 AND a_to_b = $3;
                ";
                sqlx::query(query)
                    .bind(id_a.0)
                    .bind(id_b.0)
                    .bind(a_to_b)
                    .execute(&mut tx)
                    .await?
            } else {
                let query = "
                    DELETE FROM contacts
                    WHERE user_id_a = $1 AND user_id_b = $2 AND a_to_b = $3 AND NOT accepted;
                ";
                sqlx::query(query)
                    .bind(id_a.0)
                    .bind(id_b.0)
                    .bind(a_to_b)
                    .execute(&mut tx)
                    .await?
            };
            if result.rows_affected() == 1 {
                tx.commit().await?;
                Ok(())
            } else {
                Err(anyhow!("no such contact request"))?
            }
        })
        .await
    }

    // access tokens

    pub async fn create_access_token_hash(
        &self,
        user_id: UserId,
        access_token_hash: &str,
        max_access_token_count: usize,
    ) -> Result<()> {
        self.transact(|tx| async {
            let mut tx = tx;
            let insert_query = "
                INSERT INTO access_tokens (user_id, hash)
                VALUES ($1, $2);
            ";
            let cleanup_query = "
                DELETE FROM access_tokens
                WHERE id IN (
                    SELECT id from access_tokens
                    WHERE user_id = $1
                    ORDER BY id DESC
                    LIMIT 10000
                    OFFSET $3
                )
            ";

            sqlx::query(insert_query)
                .bind(user_id.0)
                .bind(access_token_hash)
                .execute(&mut tx)
                .await?;
            sqlx::query(cleanup_query)
                .bind(user_id.0)
                .bind(access_token_hash)
                .bind(max_access_token_count as i32)
                .execute(&mut tx)
                .await?;
            Ok(tx.commit().await?)
        })
        .await
    }

    pub async fn get_access_token_hashes(&self, user_id: UserId) -> Result<Vec<String>> {
        self.transact(|mut tx| async move {
            let query = "
                SELECT hash
                FROM access_tokens
                WHERE user_id = $1
                ORDER BY id DESC
            ";
            Ok(sqlx::query_scalar(query)
                .bind(user_id.0)
                .fetch_all(&mut tx)
                .await?)
        })
        .await
    }

    async fn transact<F, Fut, T>(&self, f: F) -> Result<T>
    where
        F: Send + Fn(sqlx::Transaction<'static, D>) -> Fut,
        Fut: Send + Future<Output = Result<T>>,
    {
        let body = async {
            loop {
                let tx = self.begin_transaction().await?;
                match f(tx).await {
                    Ok(result) => return Ok(result),
                    Err(error) => match error {
                        Error::Database(error)
                            if error
                                .as_database_error()
                                .and_then(|error| error.code())
                                .as_deref()
                                == Some("40001") =>
                        {
                            // Retry (don't break the loop)
                        }
                        error @ _ => return Err(error),
                    },
                }
            }
        };

        #[cfg(test)]
        {
            if let Some(background) = self.background.as_ref() {
                background.simulate_random_delay().await;
            }

            let result = self.runtime.as_ref().unwrap().block_on(body);

            if let Some(background) = self.background.as_ref() {
                background.simulate_random_delay().await;
            }

            result
        }

        #[cfg(not(test))]
        {
            body.await
        }
    }
}

macro_rules! id_type {
    ($name:ident) => {
        #[derive(
            Clone,
            Copy,
            Debug,
            Default,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            sqlx::Type,
            Serialize,
            Deserialize,
        )]
        #[sqlx(transparent)]
        #[serde(transparent)]
        pub struct $name(pub i32);

        impl $name {
            #[allow(unused)]
            pub const MAX: Self = Self(i32::MAX);

            #[allow(unused)]
            pub fn from_proto(value: u64) -> Self {
                Self(value as i32)
            }

            #[allow(unused)]
            pub fn to_proto(self) -> u64 {
                self.0 as u64
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(UserId);
#[derive(Clone, Debug, Default, FromRow, Serialize, PartialEq)]
pub struct User {
    pub id: UserId,
    pub github_login: String,
    pub github_user_id: Option<i32>,
    pub email_address: Option<String>,
    pub admin: bool,
    pub invite_code: Option<String>,
    pub invite_count: i32,
    pub connected_once: bool,
}

id_type!(RoomId);
#[derive(Clone, Debug, Default, FromRow, Serialize, PartialEq)]
pub struct Room {
    pub id: RoomId,
    pub live_kit_room: String,
}

id_type!(ProjectId);
pub struct Project {
    pub collaborators: Vec<ProjectCollaborator>,
    pub worktrees: BTreeMap<WorktreeId, Worktree>,
    pub language_servers: Vec<proto::LanguageServer>,
}

id_type!(ReplicaId);
#[derive(Clone, Debug, Default, FromRow, PartialEq)]
pub struct ProjectCollaborator {
    pub project_id: ProjectId,
    pub connection_id: i32,
    pub user_id: UserId,
    pub replica_id: ReplicaId,
    pub is_host: bool,
}

id_type!(WorktreeId);
#[derive(Clone, Debug, Default, FromRow, PartialEq)]
struct WorktreeRow {
    pub id: WorktreeId,
    pub abs_path: String,
    pub root_name: String,
    pub visible: bool,
    pub scan_id: i64,
    pub is_complete: bool,
}

pub struct Worktree {
    pub id: WorktreeId,
    pub abs_path: String,
    pub root_name: String,
    pub visible: bool,
    pub entries: Vec<proto::Entry>,
    pub diagnostic_summaries: Vec<proto::DiagnosticSummary>,
    pub scan_id: u64,
    pub is_complete: bool,
}

#[derive(Clone, Debug, Default, FromRow, PartialEq)]
struct WorktreeEntry {
    id: i64,
    worktree_id: WorktreeId,
    is_dir: bool,
    path: String,
    inode: i64,
    mtime_seconds: i64,
    mtime_nanos: i32,
    is_symlink: bool,
    is_ignored: bool,
}

#[derive(Clone, Debug, Default, FromRow, PartialEq)]
struct WorktreeDiagnosticSummary {
    worktree_id: WorktreeId,
    path: String,
    language_server_id: i64,
    error_count: i32,
    warning_count: i32,
}

id_type!(LanguageServerId);
#[derive(Clone, Debug, Default, FromRow, PartialEq)]
struct LanguageServer {
    id: LanguageServerId,
    name: String,
}

pub struct LeftProject {
    pub id: ProjectId,
    pub host_user_id: UserId,
    pub host_connection_id: ConnectionId,
    pub connection_ids: Vec<ConnectionId>,
}

pub struct LeftRoom {
    pub room: proto::Room,
    pub left_projects: HashMap<ProjectId, LeftProject>,
    pub canceled_calls_to_user_ids: Vec<UserId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Contact {
    Accepted {
        user_id: UserId,
        should_notify: bool,
        busy: bool,
    },
    Outgoing {
        user_id: UserId,
    },
    Incoming {
        user_id: UserId,
        should_notify: bool,
    },
}

impl Contact {
    pub fn user_id(&self) -> UserId {
        match self {
            Contact::Accepted { user_id, .. } => *user_id,
            Contact::Outgoing { user_id } => *user_id,
            Contact::Incoming { user_id, .. } => *user_id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct IncomingContactRequest {
    pub requester_id: UserId,
    pub should_notify: bool,
}

#[derive(Clone, Deserialize)]
pub struct Signup {
    pub email_address: String,
    pub platform_mac: bool,
    pub platform_windows: bool,
    pub platform_linux: bool,
    pub editor_features: Vec<String>,
    pub programming_languages: Vec<String>,
    pub device_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, FromRow)]
pub struct WaitlistSummary {
    #[sqlx(default)]
    pub count: i64,
    #[sqlx(default)]
    pub linux_count: i64,
    #[sqlx(default)]
    pub mac_count: i64,
    #[sqlx(default)]
    pub windows_count: i64,
    #[sqlx(default)]
    pub unknown_count: i64,
}

#[derive(FromRow, PartialEq, Debug, Serialize, Deserialize)]
pub struct Invite {
    pub email_address: String,
    pub email_confirmation_code: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NewUserParams {
    pub github_login: String,
    pub github_user_id: i32,
    pub invite_count: i32,
}

#[derive(Debug)]
pub struct NewUserResult {
    pub user_id: UserId,
    pub metrics_id: String,
    pub inviting_user_id: Option<UserId>,
    pub signup_device_id: Option<String>,
}

fn random_invite_code() -> String {
    nanoid::nanoid!(16)
}

fn random_email_confirmation_code() -> String {
    nanoid::nanoid!(64)
}

#[cfg(test)]
pub use test::*;

#[cfg(test)]
mod test {
    use super::*;
    use gpui::executor::Background;
    use lazy_static::lazy_static;
    use parking_lot::Mutex;
    use rand::prelude::*;
    use sqlx::migrate::MigrateDatabase;
    use std::sync::Arc;

    pub struct SqliteTestDb {
        pub db: Option<Arc<Db<sqlx::Sqlite>>>,
        pub conn: sqlx::sqlite::SqliteConnection,
    }

    pub struct PostgresTestDb {
        pub db: Option<Arc<Db<sqlx::Postgres>>>,
        pub url: String,
    }

    impl SqliteTestDb {
        pub fn new(background: Arc<Background>) -> Self {
            let mut rng = StdRng::from_entropy();
            let url = format!("file:zed-test-{}?mode=memory", rng.gen::<u128>());
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
                .unwrap();

            let (mut db, conn) = runtime.block_on(async {
                let db = Db::<sqlx::Sqlite>::new(&url, 5).await.unwrap();
                let migrations_path = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations.sqlite");
                db.migrate(migrations_path.as_ref(), false).await.unwrap();
                let conn = db.pool.acquire().await.unwrap().detach();
                (db, conn)
            });

            db.background = Some(background);
            db.runtime = Some(runtime);

            Self {
                db: Some(Arc::new(db)),
                conn,
            }
        }

        pub fn db(&self) -> &Arc<Db<sqlx::Sqlite>> {
            self.db.as_ref().unwrap()
        }
    }

    impl PostgresTestDb {
        pub fn new(background: Arc<Background>) -> Self {
            lazy_static! {
                static ref LOCK: Mutex<()> = Mutex::new(());
            }

            let _guard = LOCK.lock();
            let mut rng = StdRng::from_entropy();
            let url = format!(
                "postgres://postgres@localhost/zed-test-{}",
                rng.gen::<u128>()
            );
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
                .unwrap();

            let mut db = runtime.block_on(async {
                sqlx::Postgres::create_database(&url)
                    .await
                    .expect("failed to create test db");
                let db = Db::<sqlx::Postgres>::new(&url, 5).await.unwrap();
                let migrations_path = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations");
                db.migrate(Path::new(migrations_path), false).await.unwrap();
                db
            });

            db.background = Some(background);
            db.runtime = Some(runtime);

            Self {
                db: Some(Arc::new(db)),
                url,
            }
        }

        pub fn db(&self) -> &Arc<Db<sqlx::Postgres>> {
            self.db.as_ref().unwrap()
        }
    }

    impl Drop for PostgresTestDb {
        fn drop(&mut self) {
            let db = self.db.take().unwrap();
            db.teardown(&self.url);
        }
    }
}
