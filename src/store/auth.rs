use super::*;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DatabaseScope {
    pub(super) name: String,
    pub(super) generation: String,
}

#[derive(Clone)]
pub struct Principal {
    pub hash: String,
    /// The credential's name, as the operator gave it.
    ///
    /// The hash identifies a credential uniquely but means nothing to a
    /// human; the name is what an operator reads when they ask "which
    /// key did this". It is not a secret: it is the label the operator
    /// chose when they issued the key.
    pub name: String,
    pub role: String,
    /// Names are retained for the public inventory shape; authorization uses
    /// the private generation-bound scopes below.
    pub databases: Vec<String>,
    pub(super) scopes: Vec<DatabaseScope>,
}

impl Principal {
    pub fn owner(&self) -> Result<()> {
        if self.role == "owner" {
            Ok(())
        } else {
            Err(Error::new(403, "forbidden", "owner required"))
        }
    }
    pub(super) fn database(&self, name: &str, generation: Option<&str>) -> Result<()> {
        let allowed = self.role == "owner"
            || self.scopes.iter().any(|scope| {
                scope.name == name
                    && generation
                        .map(|current| scope.generation == current)
                        .unwrap_or(true)
            });
        if allowed {
            Ok(())
        } else {
            Err(Error::new(
                403,
                "database_forbidden",
                "database access denied",
            ))
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyRequest {
    pub name: String,
    pub role: String,
    pub databases: Vec<String>,
}

pub fn hash(key: &str) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}

pub(super) fn generate_key() -> String {
    hex::encode(rand::random::<[u8; 32]>())
}

pub(super) fn migrate_legacy_scopes(conn: &Connection) -> Result<()> {
    let credentials: Vec<(String, String)> = conn
        .prepare("SELECT name,databases FROM credentials")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    for (credential, raw) in credentials {
        // One unreadable row refuses startup (fail closed), but the log
        // names the row so the operator can fix or remove exactly it.
        if let Err(error) = migrate_one_scope(conn, &credential, &raw) {
            tracing::error!(
                credential = %credential,
                code = error.code,
                "credential metadata unreadable; refusing startup"
            );
            return Err(error);
        }
    }
    Ok(())
}

fn migrate_one_scope(conn: &Connection, credential: &str, raw: &str) -> Result<()> {
    let corrupt = Error::metadata;
    let value: Value = serde_json::from_str(raw).map_err(|_| corrupt())?;
    let entries = value.as_array().ok_or_else(corrupt)?;
    // New-format rows are already generation-bound. Parsing validates
    // their shape and rejects malformed metadata rather than broadening
    // it at startup.
    if !entries.iter().all(Value::is_string) {
        parse_scopes(raw).map_err(|_| corrupt())?;
        return Ok(());
    }
    let mut scopes = Vec::with_capacity(entries.len());
    for entry in entries {
        let name = entry.as_str().ok_or_else(corrupt)?;
        if validate_name(name).is_err() {
            return Err(corrupt());
        }
        let generation: Option<String> = conn
            .query_row(
                "SELECT generation FROM databases WHERE name=?",
                [name],
                |r| r.get(0),
            )
            .optional()?;
        // A previously purged scope is retained only as a dropped access
        // grant. Omitting it is safe; inventing a generation is not.
        if let Some(generation) = generation {
            if generation.is_empty() {
                return Err(corrupt());
            }
            scopes.push(DatabaseScope {
                name: name.to_string(),
                generation,
            });
        }
    }
    let encoded = serde_json::to_string(&scopes).map_err(|_| corrupt())?;
    conn.execute(
        "UPDATE credentials SET databases=? WHERE name=?",
        params![encoded, credential],
    )?;
    Ok(())
}

fn parse_scopes(raw: &str) -> Result<Vec<DatabaseScope>> {
    let scopes: Vec<DatabaseScope> = serde_json::from_str(raw).map_err(|_| Error::internal())?;
    if scopes
        .iter()
        .any(|scope| scope.generation.is_empty() || validate_name(&scope.name).is_err())
    {
        return Err(Error::internal());
    }
    Ok(scopes)
}

fn scope_names(scopes: &[DatabaseScope]) -> Vec<String> {
    scopes.iter().map(|scope| scope.name.clone()).collect()
}

/// Only a uniqueness violation means the name is taken; every other
/// insert failure (I/O, corruption) keeps its underlying mapping instead
/// of masquerading as a duplicate name.
pub(super) fn map_issue_error(error: rusqlite::Error) -> Error {
    if error.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) {
        Error::new(409, "key_exists", "credential name already exists")
    } else {
        error.into()
    }
}

pub(super) fn new_generation(conn: &Connection) -> Result<String> {
    for _ in 0..16 {
        let generation = hex::encode(rand::random::<[u8; 16]>());
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM databases WHERE generation=?)",
            [&generation],
            |r| r.get(0),
        )?;
        if !exists {
            return Ok(generation);
        }
    }
    Err(Error::internal())
}

pub fn unauthorized() -> Error {
    Error::new(
        401,
        "invalid_credential",
        "valid bearer credential required",
    )
}

impl Store {
    pub fn bootstrap(&self) -> Result<String> {
        let mut conn = self.meta()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if tx.query_row(
            "SELECT count(*) FROM credentials WHERE role='owner'",
            [],
            |r| r.get::<_, i64>(0),
        )? > 0
        {
            return Err(Error::new(
                409,
                "already_bootstrapped",
                "owner already exists",
            ));
        }
        let key = generate_key();
        tx.execute(
            "INSERT INTO credentials VALUES ('owner',?,'owner','[]')",
            [hash(&key)],
        )?;
        tx.commit()?;
        Ok(key)
    }

    pub fn authenticate(&self, key: &str) -> Result<Principal> {
        if key.len() != 64 {
            return Err(unauthorized());
        }
        let hash = hash(key);
        let result = self.meta()?.query_row(
            "SELECT name,role,databases FROM credentials WHERE hash=?",
            [&hash],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        );
        match result {
            Ok((name, role, dbs)) => {
                let scopes = parse_scopes(&dbs)?;
                Ok(Principal {
                    hash,
                    name,
                    role,
                    databases: scope_names(&scopes),
                    scopes,
                })
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Err(unauthorized()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn issue(&self, request: KeyRequest) -> Result<String> {
        validate_name(&request.name)?;
        if request.name == "owner"
            || !matches!(
                request.role.as_str(),
                "database_read_only" | "database_read_write"
            )
            || request.databases.is_empty()
            || request.databases.len() > 32
        {
            return Err(Error::invalid());
        }
        for name in &request.databases {
            validate_name(name)?;
        }
        let mut conn = self.meta()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if tx.query_row("SELECT count(*) FROM credentials", [], |r| {
            r.get::<_, i64>(0)
        })? >= 256
        {
            return Err(Error::busy());
        }
        let mut scopes = Vec::with_capacity(request.databases.len());
        for name in &request.databases {
            let generation: Option<String> = tx
                .query_row(
                    "SELECT generation FROM databases WHERE name=? AND deleted_at IS NULL",
                    [name],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(generation) = generation else {
                return Err(Error::new(404, "database_not_found", "database not found"));
            };
            if generation.is_empty() {
                return Err(Error::internal());
            }
            scopes.push(DatabaseScope {
                name: name.clone(),
                generation,
            });
        }
        let key = generate_key();
        tx.execute(
            "INSERT INTO credentials VALUES (?,?,?,?)",
            params![
                request.name,
                hash(&key),
                request.role,
                serde_json::to_string(&scopes).map_err(|_| Error::internal())?
            ],
        )
        .map_err(map_issue_error)?;
        tx.commit()?;
        Ok(key)
    }

    /// Owner-visible credential inventory: name, role, and scope.
    /// Hashes never leave the server; the owner row itself is listed.
    pub fn list_keys(&self) -> Result<serde_json::Value> {
        let conn = self.meta()?;
        let mut stmt =
            conn.prepare("SELECT name, role, databases FROM credentials ORDER BY name")?;
        let rows: Vec<(String, String, String)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<_, _>>()?;
        let mut keys = Vec::with_capacity(rows.len());
        for (name, role, databases) in rows {
            let scopes = parse_scopes(&databases)?;
            keys.push(serde_json::json!({
                "name": name,
                "role": role,
                "databases": scope_names(&scopes),
            }));
        }
        Ok(serde_json::json!({"keys": keys}))
    }

    pub fn revoke(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        if name == "owner" {
            return Err(Error::invalid());
        }
        let changed = self.meta()?.execute(
            "DELETE FROM credentials WHERE name=? AND role != 'owner'",
            [name],
        )?;
        if changed == 0 {
            return Err(Error::new(
                404,
                "key_not_found",
                "no such credential; nothing revoked",
            ));
        }
        Ok(())
    }

    /// Revoke every credential scoped to one database, atomically.
    ///
    /// This is the bulk form of `revoke`, for the case it exists for:
    /// a key scoped to one database has leaked, and cutting it must not
    /// depend on first remembering its name. It answers the same
    /// question `tokens` (the read side) asks — who can reach this
    /// database — and revokes exactly that set.
    ///
    /// The owner key is never in that set. It reaches every database by
    /// construction, so sweeping it up would revoke the credential the
    /// operator is holding — locking them out of the gateway they were
    /// trying to protect. `revoke` refuses the owner by name; this
    /// refuses it by role, which is the property that actually matters
    /// here.
    ///
    /// Scopes are compared by database name, not by generation: the
    /// question is "which credentials can reach this database", and a
    /// stale generation on a leaked key is still a leaked key. Returns
    /// the revoked names, so the caller can report what it cut rather
    /// than how many.
    pub fn revoke_scoped_to(&self, database: &str) -> Result<Vec<String>> {
        validate_name(database)?;
        let mut conn = self.meta()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let rows: Vec<(String, String)> = {
            let mut stmt =
                tx.prepare("SELECT name, databases FROM credentials WHERE role != 'owner'")?;
            stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<_, _>>()?
        };
        let mut revoked = Vec::new();
        for (name, scopes) in rows {
            let scopes = parse_scopes(&scopes)?;
            if scopes.iter().any(|scope| scope.name == database) {
                revoked.push(name);
            }
        }
        for name in &revoked {
            tx.execute(
                "DELETE FROM credentials WHERE name=? AND role != 'owner'",
                [name],
            )?;
        }
        tx.commit()?;
        Ok(revoked)
    }

    /// Replace the owner credential atomically and return the new raw key
    /// (shown once). The old key stops authenticating immediately; database
    /// rows, generations, and app-key scopes are untouched. Only reachable
    /// with a valid owner key, so a missing owner row is corruption, not a
    /// client error.
    pub fn rotate_owner(&self) -> Result<String> {
        let mut conn = self.meta()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let key = generate_key();
        let changed = tx.execute(
            "UPDATE credentials SET hash=? WHERE name='owner' AND role='owner'",
            [hash(&key)],
        )?;
        if changed != 1 {
            return Err(Error::internal());
        }
        tx.commit()?;
        Ok(key)
    }

    /// Authorize a principal against the database's current generation.
    /// Missing rows are left to `path()` so a scoped credential retains the
    /// existing 404 behavior for a purged database, while a replacement with
    /// a different generation is denied before SQLite is opened.
    pub fn authorize(&self, principal: &Principal, name: &str) -> Result<()> {
        if principal.role == "owner" {
            return Ok(());
        }
        let generation = self.generation_for(name)?;
        principal.database(name, generation.as_deref())
    }

    fn generation_for(&self, name: &str) -> Result<Option<String>> {
        validate_name(name)?;
        let row: Option<Option<String>> = self
            .meta()?
            .query_row(
                "SELECT generation FROM databases WHERE name=?",
                [name],
                |r| r.get(0),
            )
            .optional()?;
        match row {
            Some(Some(generation)) if !generation.is_empty() => Ok(Some(generation)),
            Some(Some(_)) | Some(None) => Err(Error::internal()),
            None => Ok(None),
        }
    }
}
