use crate::util::LowerCase;
use crate::CONFIG;
use chrono::{NaiveDateTime, TimeDelta, Utc};
use derive_more::{AsRef, Deref, Display, From};
use serde_json::Value;

use super::{
    Attachment, CollectionCipher, CollectionId, Favorite, FolderCipher, FolderId, Group, Membership, MembershipStatus,
    MembershipType, OrganizationId, User, UserId,
};
use crate::api::core::{CipherData, CipherSyncData, CipherSyncType};
use macros::UuidFromParam;

use std::borrow::Cow;

db_object! {
    #[derive(Identifiable, Queryable, Insertable, AsChangeset)]
    #[diesel(table_name = ciphers)]
    #[diesel(treat_none_as_null = true)]
    #[diesel(primary_key(uuid))]
    pub struct Cipher {
        pub uuid: CipherId,
        pub created_at: NaiveDateTime,
        pub updated_at: NaiveDateTime,

        pub user_uuid: Option<UserId>,
        pub organization_uuid: Option<OrganizationId>,

        pub key: Option<String>,

        /*
        Login = 1,
        SecureNote = 2,
        Card = 3,
        Identity = 4,
        SshKey = 5
        */
        pub atype: i32,
        pub name: String,
        pub notes: Option<String>,
        pub fields: Option<String>,

        pub data: String,

        pub password_history: Option<String>,
        pub deleted_at: Option<NaiveDateTime>,
        pub reprompt: Option<i32>,
    }
}

pub enum RepromptType {
    None = 0,
    Password = 1,
}

/// Local methods
impl Cipher {
    pub fn new(atype: i32, name: String) -> Self {
        let now = Utc::now().naive_utc();

        Self {
            uuid: CipherId(crate::util::get_uuid()),
            created_at: now,
            updated_at: now,

            user_uuid: None,
            organization_uuid: None,

            key: None,

            atype,
            name,

            notes: None,
            fields: None,

            data: String::new(),
            password_history: None,
            deleted_at: None,
            reprompt: None,
        }
    }

    pub fn validate_cipher_data(cipher_data: &[CipherData]) -> EmptyResult {
        let mut validation_errors = serde_json::Map::new();
        let max_note_size = CONFIG._max_note_size();
        let max_note_size_msg =
            format!("The field Notes exceeds the maximum encrypted value length of {max_note_size} characters.");
        for (index, cipher) in cipher_data.iter().enumerate() {
            // Validate the note size and if it is exceeded return a warning
            if let Some(note) = &cipher.notes {
                if note.len() > max_note_size {
                    validation_errors
                        .insert(format!("Ciphers[{index}].Notes"), serde_json::to_value([&max_note_size_msg]).unwrap());
                }
            }

            // Validate the password history if it contains `null` values and if so, return a warning
            if let Some(Value::Array(password_history)) = &cipher.password_history {
                for pwh in password_history {
                    if let Value::Object(pwo) = pwh {
                        if pwo.get("password").is_some_and(|p| !p.is_string()) {
                            validation_errors.insert(
                                format!("Ciphers[{index}].Notes"),
                                serde_json::to_value([
                                    "The password history contains a `null` value. Only strings are allowed.",
                                ])
                                .unwrap(),
                            );
                            break;
                        }
                    }
                }
            }
        }

        if !validation_errors.is_empty() {
            let err_json = json!({
                "message": "The model state is invalid.",
                "validationErrors" : validation_errors,
                "object": "error"
            });
            err_json!(err_json, "Import validation errors")
        } else {
            Ok(())
        }
    }
}

use crate::db::DbConn;

use crate::api::EmptyResult;
use crate::error::MapResult;

/// Database methods
impl Cipher {
    pub async fn to_json(
        &self,
        host: &str,
        user_uuid: &UserId,
        cipher_sync_data: Option<&CipherSyncData>,
        sync_type: CipherSyncType,
        conn: &mut DbConn,
    ) -> Result<Value, crate::Error> {
        use crate::util::{format_date, validate_and_format_date};

        let mut attachments_json: Value = Value::Null;
        if let Some(cipher_sync_data) = cipher_sync_data {
            if let Some(attachments) = cipher_sync_data.cipher_attachments.get(&self.uuid) {
                if !attachments.is_empty() {
                    let mut attachments_json_vec = vec![];
                    for attachment in attachments {
                        attachments_json_vec.push(attachment.to_json(host).await?);
                    }
                    attachments_json = Value::Array(attachments_json_vec);
                }
            }
        } else {
            let attachments = Attachment::find_by_cipher(&self.uuid, conn).await;
            if !attachments.is_empty() {
                let mut attachments_json_vec = vec![];
                for attachment in attachments {
                    attachments_json_vec.push(attachment.to_json(host).await?);
                }
                attachments_json = Value::Array(attachments_json_vec);
            }
        }

        // We don't need these values at all for Organizational syncs
        // Skip any other database calls if this is the case and just return false.
        let (read_only, hide_passwords, _) = if sync_type == CipherSyncType::User {
            match self.get_access_restrictions(user_uuid, cipher_sync_data, conn).await {
                Some((ro, hp, mn)) => (ro, hp, mn),
                None => {
                    error!("Cipher ownership assertion failure");
                    (true, true, false)
                }
            }
        } else {
            (false, false, false)
        };

        let fields_json: Vec<_> = self
            .fields
            .as_ref()
            .and_then(|s| {
                serde_json::from_str::<Vec<LowerCase<Value>>>(s)
                    .inspect_err(|e| warn!("Error parsing fields {e:?} for {}", self.uuid))
                    .ok()
            })
            .map(|d| {
                d.into_iter()
                    .map(|mut f| {
                        // Check if the `type` key is a number, strings break some clients
                        // The fallback type is the hidden type `1`. this should prevent accidental data disclosure
                        // If not try to convert the string value to a number and fallback to `1`
                        // If it is both not a number and not a string, fallback to `1`
                        match f.data.get("type") {
                            Some(t) if t.is_number() => {}
                            Some(t) if t.is_string() => {
                                let type_num = &t.as_str().unwrap_or("1").parse::<u8>().unwrap_or(1);
                                f.data["type"] = json!(type_num);
                            }
                            _ => {
                                f.data["type"] = json!(1);
                            }
                        }
                        f.data
                    })
                    .collect()
            })
            .unwrap_or_default();

        let password_history_json: Vec<_> = self
            .password_history
            .as_ref()
            .and_then(|s| {
                serde_json::from_str::<Vec<LowerCase<Value>>>(s)
                    .inspect_err(|e| warn!("Error parsing password history {e:?} for {}", self.uuid))
                    .ok()
            })
            .map(|d| {
                // Check every password history item if they are valid and return it.
                // If a password field has the type `null` skip it, it breaks newer Bitwarden clients
                // A second check is done to verify the lastUsedDate exists and is a valid DateTime string, if not the epoch start time will be used
                d.into_iter()
                    .filter_map(|d| match d.data.get("password") {
                        Some(p) if p.is_string() => Some(d.data),
                        _ => None,
                    })
                    .map(|mut d| match d.get("lastUsedDate").and_then(|l| l.as_str()) {
                        Some(l) => {
                            d["lastUsedDate"] = json!(validate_and_format_date(l));
                            d
                        }
                        _ => {
                            d["lastUsedDate"] = json!("1970-01-01T00:00:00.000000Z");
                            d
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Get the type_data or a default to an empty json object '{}'.
        // If not passing an empty object, mobile clients will crash.
        let mut type_data_json =
            serde_json::from_str::<LowerCase<Value>>(&self.data).map(|d| d.data).unwrap_or_else(|_| {
                warn!("Error parsing data field for {}", self.uuid);
                Value::Object(serde_json::Map::new())
            });

        // NOTE: This was marked as *Backwards Compatibility Code*, but as of January 2021 this is still being used by upstream
        // Set the first element of the Uris array as Uri, this is needed several (mobile) clients.
        if self.atype == 1 {
            // Upstream always has an `uri` key/value
            type_data_json["uri"] = Value::Null;
            if let Some(uris) = type_data_json["uris"].as_array_mut() {
                if !uris.is_empty() {
                    // Fix uri match values first, they are only allowed to be a number or null
                    // If it is a string, convert it to an int or null if that fails
                    for uri in &mut *uris {
                        if uri["match"].is_string() {
                            let match_value = match uri["match"].as_str().unwrap_or_default().parse::<u8>() {
                                Ok(n) => json!(n),
                                _ => Value::Null,
                            };
                            uri["match"] = match_value;
                        }
                    }
                    type_data_json["uri"] = uris[0]["uri"].clone();
                }
            }

            // Check if `passwordRevisionDate` is a valid date, else convert it
            if let Some(pw_revision) = type_data_json["passwordRevisionDate"].as_str() {
                type_data_json["passwordRevisionDate"] = json!(validate_and_format_date(pw_revision));
            }
        }

        // Fix secure note issues when data is invalid
        // This breaks at least the native mobile clients
        if self.atype == 2 {
            match type_data_json {
                Value::Object(ref t) if t.get("type").is_some_and(|t| t.is_number()) => {}
                _ => {
                    type_data_json = json!({"type": 0});
                }
            }
        }

        // Fix invalid SSH Entries
        // This breaks at least the native mobile client if invalid
        // The only way to fix this is by setting type_data_json to `null`
        // Opening this ssh-key in the mobile client will probably crash the client, but you can edit, save and afterwards delete it
        if self.atype == 5
            && (type_data_json["keyFingerprint"].as_str().is_none_or(|v| v.is_empty())
                || type_data_json["privateKey"].as_str().is_none_or(|v| v.is_empty())
                || type_data_json["publicKey"].as_str().is_none_or(|v| v.is_empty()))
        {
            warn!("Error parsing ssh-key, mandatory fields are invalid for {}", self.uuid);
            type_data_json = Value::Null;
        }

        // Clone the type_data and add some default value.
        let mut data_json = type_data_json.clone();

        // NOTE: This was marked as *Backwards Compatibility Code*, but as of January 2021 this is still being used by upstream
        // data_json should always contain the following keys with every atype
        data_json["fields"] = json!(fields_json);
        data_json["name"] = json!(self.name);
        data_json["notes"] = json!(self.notes);
        data_json["passwordHistory"] = Value::Array(password_history_json.clone());

        let collection_ids = if let Some(cipher_sync_data) = cipher_sync_data {
            if let Some(cipher_collections) = cipher_sync_data.cipher_collections.get(&self.uuid) {
                Cow::from(cipher_collections)
            } else {
                Cow::from(Vec::with_capacity(0))
            }
        } else {
            Cow::from(self.get_admin_collections(user_uuid.clone(), conn).await)
        };

        // There are three types of cipher response models in upstream
        // Bitwarden: "cipherMini", "cipher", and "cipherDetails" (in order
        // of increasing level of detail). vaultwarden currently only
        // supports the "cipherDetails" type, though it seems like the
        // Bitwarden clients will ignore extra fields.
        //
        // Ref: https://github.com/bitwarden/server/blob/9ebe16587175b1c0e9208f84397bb75d0d595510/src/Api/Vault/Models/Response/CipherResponseModel.cs#L14
        let mut json_object = json!({
            "object": "cipherDetails",
            "id": self.uuid,
            "type": self.atype,
            "creationDate": format_date(&self.created_at),
            "revisionDate": format_date(&self.updated_at),
            "deletedDate": self.deleted_at.map_or(Value::Null, |d| Value::String(format_date(&d))),
            "reprompt": self.reprompt.filter(|r| *r == RepromptType::None as i32 || *r == RepromptType::Password as i32).unwrap_or(RepromptType::None as i32),
            "organizationId": self.organization_uuid,
            "key": self.key,
            "attachments": attachments_json,
            // We have UseTotp set to true by default within the Organization model.
            // This variable together with UsersGetPremium is used to show or hide the TOTP counter.
            "organizationUseTotp": true,

            // This field is specific to the cipherDetails type.
            "collectionIds": collection_ids,

            "name": self.name,
            "notes": self.notes,
            "fields": fields_json,

            "data": data_json,

            "passwordHistory": password_history_json,

            // All Cipher types are included by default as null, but only the matching one will be populated
            "login": null,
            "secureNote": null,
            "card": null,
            "identity": null,
            "sshKey": null,
        });

        // These values are only needed for user/default syncs
        // Not during an organizational sync like `get_org_details`
        // Skip adding these fields in that case
        if sync_type == CipherSyncType::User {
            json_object["folderId"] = json!(if let Some(cipher_sync_data) = cipher_sync_data {
                cipher_sync_data.cipher_folders.get(&self.uuid).cloned()
            } else {
                self.get_folder_uuid(user_uuid, conn).await
            });
            json_object["favorite"] = json!(if let Some(cipher_sync_data) = cipher_sync_data {
                cipher_sync_data.cipher_favorites.contains(&self.uuid)
            } else {
                self.is_favorite(user_uuid, conn).await
            });
            // These values are true by default, but can be false if the
            // cipher belongs to a collection or group where the org owner has enabled
            // the "Read Only" or "Hide Passwords" restrictions for the user.
            json_object["edit"] = json!(!read_only);
            json_object["viewPassword"] = json!(!hide_passwords);
            // The new key used by clients since v2025.6.0
            json_object["permissions"] = json!({
                "delete": !read_only,
                "restore": !read_only,
            });
        }

        let key = match self.atype {
            1 => "login",
            2 => "secureNote",
            3 => "card",
            4 => "identity",
            5 => "sshKey",
            _ => panic!("Wrong type"),
        };

        json_object[key] = type_data_json;
        Ok(json_object)
    }

    pub async fn update_users_revision(&self, conn: &mut DbConn) -> Vec<UserId> {
        let mut user_uuids = Vec::new();
        match self.user_uuid {
            Some(ref user_uuid) => {
                User::update_uuid_revision(user_uuid, conn).await;
                user_uuids.push(user_uuid.clone())
            }
            None => {
                // Belongs to Organization, need to update affected users
                if let Some(ref org_uuid) = self.organization_uuid {
                    // users having access to the collection
                    let mut collection_users = Membership::find_by_cipher_and_org(&self.uuid, org_uuid, conn).await;
                    if CONFIG.org_groups_enabled() {
                        // members of a group having access to the collection
                        let group_users =
                            Membership::find_by_cipher_and_org_with_group(&self.uuid, org_uuid, conn).await;
                        collection_users.extend(group_users);
                    }
                    for member in collection_users {
                        User::update_uuid_revision(&member.user_uuid, conn).await;
                        user_uuids.push(member.user_uuid.clone())
                    }
                }
            }
        };
        user_uuids
    }

    pub async fn save(&mut self, conn: &mut DbConn) -> EmptyResult {
        self.update_users_revision(conn).await;
        self.updated_at = Utc::now().naive_utc();

        db_run! { conn:
            sqlite, mysql {
                match diesel::replace_into(ciphers::table)
                    .values(CipherDb::to_db(self))
                    .execute(conn)
                {
                    Ok(_) => Ok(()),
                    // Record already exists and causes a Foreign Key Violation because replace_into() wants to delete the record first.
                    Err(diesel::result::Error::DatabaseError(diesel::result::DatabaseErrorKind::ForeignKeyViolation, _)) => {
                        diesel::update(ciphers::table)
                            .filter(ciphers::uuid.eq(&self.uuid))
                            .set(CipherDb::to_db(self))
                            .execute(conn)
                            .map_res("Error saving cipher")
                    }
                    Err(e) => Err(e.into()),
                }.map_res("Error saving cipher")
            }
            postgresql {
                let value = CipherDb::to_db(self);
                diesel::insert_into(ciphers::table)
                    .values(&value)
                    .on_conflict(ciphers::uuid)
                    .do_update()
                    .set(&value)
                    .execute(conn)
                    .map_res("Error saving cipher")
            }
        }
    }

    pub async fn delete(&self, conn: &mut DbConn) -> EmptyResult {
        self.update_users_revision(conn).await;

        FolderCipher::delete_all_by_cipher(&self.uuid, conn).await?;
        CollectionCipher::delete_all_by_cipher(&self.uuid, conn).await?;
        Attachment::delete_all_by_cipher(&self.uuid, conn).await?;
        Favorite::delete_all_by_cipher(&self.uuid, conn).await?;

        db_run! { conn: {
            diesel::delete(ciphers::table.filter(ciphers::uuid.eq(&self.uuid)))
                .execute(conn)
                .map_res("Error deleting cipher")
        }}
    }

    pub async fn delete_all_by_organization(org_uuid: &OrganizationId, conn: &mut DbConn) -> EmptyResult {
        // TODO: Optimize this by executing a DELETE directly on the database, instead of first fetching.
        for cipher in Self::find_by_org(org_uuid, conn).await {
            cipher.delete(conn).await?;
        }
        Ok(())
    }

    pub async fn delete_all_by_user(user_uuid: &UserId, conn: &mut DbConn) -> EmptyResult {
        for cipher in Self::find_owned_by_user(user_uuid, conn).await {
            cipher.delete(conn).await?;
        }
        Ok(())
    }

    /// Purge all ciphers that are old enough to be auto-deleted.
    pub async fn purge_trash(conn: &mut DbConn) {
        if let Some(auto_delete_days) = CONFIG.trash_auto_delete_days() {
            let now = Utc::now().naive_utc();
            let dt = now - TimeDelta::try_days(auto_delete_days).unwrap();
            for cipher in Self::find_deleted_before(&dt, conn).await {
                cipher.delete(conn).await.ok();
            }
        }
    }

    pub async fn move_to_folder(
        &self,
        folder_uuid: Option<FolderId>,
        user_uuid: &UserId,
        conn: &mut DbConn,
    ) -> EmptyResult {
        User::update_uuid_revision(user_uuid, conn).await;

        match (self.get_folder_uuid(user_uuid, conn).await, folder_uuid) {
            // No changes
            (None, None) => Ok(()),
            (Some(ref old_folder), Some(ref new_folder)) if old_folder == new_folder => Ok(()),

            // Add to folder
            (None, Some(new_folder)) => FolderCipher::new(new_folder, self.uuid.clone()).save(conn).await,

            // Remove from folder
            (Some(old_folder), None) => {
                match FolderCipher::find_by_folder_and_cipher(&old_folder, &self.uuid, conn).await {
                    Some(old_folder) => old_folder.delete(conn).await,
                    None => err!("Couldn't move from previous folder"),
                }
            }

            // Move to another folder
            (Some(old_folder), Some(new_folder)) => {
                if let Some(old_folder) = FolderCipher::find_by_folder_and_cipher(&old_folder, &self.uuid, conn).await {
                    old_folder.delete(conn).await?;
                }
                FolderCipher::new(new_folder, self.uuid.clone()).save(conn).await
            }
        }
    }

    /// Returns whether this cipher is directly owned by the user.
    pub fn is_owned_by_user(&self, user_uuid: &UserId) -> bool {
        self.user_uuid.is_some() && self.user_uuid.as_ref().unwrap() == user_uuid
    }

    /// Returns whether this cipher is owned by an org in which the user has full access.
    async fn is_in_full_access_org(
        &self,
        user_uuid: &UserId,
        cipher_sync_data: Option<&CipherSyncData>,
        conn: &mut DbConn,
    ) -> bool {
        if let Some(ref org_uuid) = self.organization_uuid {
            if let Some(cipher_sync_data) = cipher_sync_data {
                if let Some(cached_member) = cipher_sync_data.members.get(org_uuid) {
                    return cached_member.has_full_access();
                }
            } else if let Some(member) = Membership::find_by_user_and_org(user_uuid, org_uuid, conn).await {
                return member.has_full_access();
            }
        }
        false
    }

    /// Returns whether this cipher is owned by an group in which the user has full access.
    async fn is_in_full_access_group(
        &self,
        user_uuid: &UserId,
        cipher_sync_data: Option<&CipherSyncData>,
        conn: &mut DbConn,
    ) -> bool {
        if !CONFIG.org_groups_enabled() {
            return false;
        }
        if let Some(ref org_uuid) = self.organization_uuid {
            if let Some(cipher_sync_data) = cipher_sync_data {
                return cipher_sync_data.user_group_full_access_for_organizations.contains(org_uuid);
            } else {
                return Group::is_in_full_access_group(user_uuid, org_uuid, conn).await;
            }
        }
        false
    }

    /// Returns the user's access restrictions to this cipher. A return value
    /// of None means that this cipher does not belong to the user, and is
    /// not in any collection the user has access to. Otherwise, the user has
    /// access to this cipher, and Some(read_only, hide_passwords, manage) represents
    /// the access restrictions.
    pub async fn get_access_restrictions(
        &self,
        user_uuid: &UserId,
        cipher_sync_data: Option<&CipherSyncData>,
        conn: &mut DbConn,
    ) -> Option<(bool, bool, bool)> {
        // Check whether this cipher is directly owned by the user, or is in
        // a collection that the user has full access to. If so, there are no
        // access restrictions.
        if self.is_owned_by_user(user_uuid)
            || self.is_in_full_access_org(user_uuid, cipher_sync_data, conn).await
            || self.is_in_full_access_group(user_uuid, cipher_sync_data, conn).await
        {
            return Some((false, false, true));
        }

        let rows = if let Some(cipher_sync_data) = cipher_sync_data {
            let mut rows: Vec<(bool, bool, bool)> = Vec::new();
            if let Some(collections) = cipher_sync_data.cipher_collections.get(&self.uuid) {
                for collection in collections {
                    // User permissions
                    if let Some(cu) = cipher_sync_data.user_collections.get(collection) {
                        rows.push((cu.read_only, cu.hide_passwords, cu.manage));
                    // Group permissions
                    } else if let Some(cg) = cipher_sync_data.user_collections_groups.get(collection) {
                        rows.push((cg.read_only, cg.hide_passwords, cg.manage));
                    }
                }
            }
            rows
        } else {
            let user_permissions = self.get_user_collections_access_flags(user_uuid, conn).await;
            if !user_permissions.is_empty() {
                user_permissions
            } else {
                self.get_group_collections_access_flags(user_uuid, conn).await
            }
        };

        if rows.is_empty() {
            // This cipher isn't in any collections accessible to the user.
            return None;
        }

        // A cipher can be in multiple collections with inconsistent access flags.
        // Also, user permission overrule group permissions
        // and only user permissions are returned by the code above.
        //
        // For example, a cipher could be in one collection where the user has
        // read-only access, but also in another collection where the user has
        // read/write access. For a flag to be in effect for a cipher, upstream
        // requires all collections the cipher is in to have that flag set.
        // Therefore, we do a boolean AND of all values in each of the `read_only`
        // and `hide_passwords` columns. This could ideally be done as part of the
        // query, but Diesel doesn't support a min() or bool_and() function on
        // booleans and this behavior isn't portable anyway.
        //
        // The only exception is for the `manage` flag, that needs a boolean OR!
        let mut read_only = true;
        let mut hide_passwords = true;
        let mut manage = false;
        for (ro, hp, mn) in rows.iter() {
            read_only &= ro;
            hide_passwords &= hp;
            manage |= mn;
        }

        Some((read_only, hide_passwords, manage))
    }

    async fn get_user_collections_access_flags(
        &self,
        user_uuid: &UserId,
        conn: &mut DbConn,
    ) -> Vec<(bool, bool, bool)> {
        db_run! {conn: {
            // Check whether this cipher is in any collections accessible to the
            // user. If so, retrieve the access flags for each collection.
            ciphers::table
                .filter(ciphers::uuid.eq(&self.uuid))
                .inner_join(ciphers_collections::table.on(
                    ciphers::uuid.eq(ciphers_collections::cipher_uuid)))
                .inner_join(users_collections::table.on(
                    ciphers_collections::collection_uuid.eq(users_collections::collection_uuid)
                        .and(users_collections::user_uuid.eq(user_uuid))))
                .select((users_collections::read_only, users_collections::hide_passwords, users_collections::manage))
                .load::<(bool, bool, bool)>(conn)
                .expect("Error getting user access restrictions")
        }}
    }

    async fn get_group_collections_access_flags(
        &self,
        user_uuid: &UserId,
        conn: &mut DbConn,
    ) -> Vec<(bool, bool, bool)> {
        if !CONFIG.org_groups_enabled() {
            return Vec::new();
        }
        db_run! {conn: {
            ciphers::table
                .filter(ciphers::uuid.eq(&self.uuid))
                .inner_join(ciphers_collections::table.on(
                    ciphers::uuid.eq(ciphers_collections::cipher_uuid)
                ))
                .inner_join(collections_groups::table.on(
                    collections_groups::collections_uuid.eq(ciphers_collections::collection_uuid)
                ))
                .inner_join(groups_users::table.on(
                    groups_users::groups_uuid.eq(collections_groups::groups_uuid)
                ))
                .inner_join(users_organizations::table.on(
                    users_organizations::uuid.eq(groups_users::users_organizations_uuid)
                ))
                .filter(users_organizations::user_uuid.eq(user_uuid))
                .select((collections_groups::read_only, collections_groups::hide_passwords, collections_groups::manage))
                .load::<(bool, bool, bool)>(conn)
                .expect("Error getting group access restrictions")
        }}
    }

    pub async fn is_write_accessible_to_user(&self, user_uuid: &UserId, conn: &mut DbConn) -> bool {
        match self.get_access_restrictions(user_uuid, None, conn).await {
            Some((read_only, _hide_passwords, manage)) => !read_only || manage,
            None => false,
        }
    }

    pub async fn is_accessible_to_user(&self, user_uuid: &UserId, conn: &mut DbConn) -> bool {
        self.get_access_restrictions(user_uuid, None, conn).await.is_some()
    }

    // Returns whether this cipher is a favorite of the specified user.
    pub async fn is_favorite(&self, user_uuid: &UserId, conn: &mut DbConn) -> bool {
        Favorite::is_favorite(&self.uuid, user_uuid, conn).await
    }

    // Sets whether this cipher is a favorite of the specified user.
    pub async fn set_favorite(&self, favorite: Option<bool>, user_uuid: &UserId, conn: &mut DbConn) -> EmptyResult {
        match favorite {
            None => Ok(()), // No change requested.
            Some(status) => Favorite::set_favorite(status, &self.uuid, user_uuid, conn).await,
        }
    }

    pub async fn get_folder_uuid(&self, user_uuid: &UserId, conn: &mut DbConn) -> Option<FolderId> {
        db_run! {conn: {
            folders_ciphers::table
                .inner_join(folders::table)
                .filter(folders::user_uuid.eq(&user_uuid))
                .filter(folders_ciphers::cipher_uuid.eq(&self.uuid))
                .select(folders_ciphers::folder_uuid)
                .first::<FolderId>(conn)
                .ok()
        }}
    }

    pub async fn find_by_uuid(uuid: &CipherId, conn: &mut DbConn) -> Option<Self> {
        db_run! {conn: {
            ciphers::table
                .filter(ciphers::uuid.eq(uuid))
                .first::<CipherDb>(conn)
                .ok()
                .from_db()
        }}
    }

    pub async fn find_by_uuid_and_org(
        cipher_uuid: &CipherId,
        org_uuid: &OrganizationId,
        conn: &mut DbConn,
    ) -> Option<Self> {
        db_run! {conn: {
            ciphers::table
                .filter(ciphers::uuid.eq(cipher_uuid))
                .filter(ciphers::organization_uuid.eq(org_uuid))
                .first::<CipherDb>(conn)
                .ok()
                .from_db()
        }}
    }

    // Find all ciphers accessible or visible to the specified user.
    //
    // "Accessible" means the user has read access to the cipher, either via
    // direct ownership, collection or via group access.
    //
    // "Visible" usually means the same as accessible, except when an org
    // owner/admin sets their account or group to have access to only selected
    // collections in the org (presumably because they aren't interested in
    // the other collections in the org). In this case, if `visible_only` is
    // true, then the non-interesting ciphers will not be returned. As a
    // result, those ciphers will not appear in "My Vault" for the org
    // owner/admin, but they can still be accessed via the org vault view.
    pub async fn find_by_user(user_uuid: &UserId, visible_only: bool, conn: &mut DbConn) -> Vec<Self> {
        if CONFIG.org_groups_enabled() {
            db_run! {conn: {
                let mut query = ciphers::table
                    .left_join(ciphers_collections::table.on(
                            ciphers::uuid.eq(ciphers_collections::cipher_uuid)
                            ))
                    .left_join(users_organizations::table.on(
                            ciphers::organization_uuid.eq(users_organizations::org_uuid.nullable())
                            .and(users_organizations::user_uuid.eq(user_uuid))
                            .and(users_organizations::status.eq(MembershipStatus::Confirmed as i32))
                            ))
                    .left_join(users_collections::table.on(
                            ciphers_collections::collection_uuid.eq(users_collections::collection_uuid)
                            // Ensure that users_collections::user_uuid is NULL for unconfirmed users.
                            .and(users_organizations::user_uuid.eq(users_collections::user_uuid))
                            ))
                    .left_join(groups_users::table.on(
                            groups_users::users_organizations_uuid.eq(users_organizations::uuid)
                            ))
                    .left_join(groups::table.on(
                            groups::uuid.eq(groups_users::groups_uuid)
                            ))
                    .left_join(collections_groups::table.on(
                            collections_groups::collections_uuid.eq(ciphers_collections::collection_uuid).and(
                                collections_groups::groups_uuid.eq(groups::uuid)
                                )
                            ))
                    .filter(ciphers::user_uuid.eq(user_uuid)) // Cipher owner
                    .or_filter(users_organizations::access_all.eq(true)) // access_all in org
                    .or_filter(users_collections::user_uuid.eq(user_uuid)) // Access to collection
                    .or_filter(groups::access_all.eq(true)) // Access via groups
                    .or_filter(collections_groups::collections_uuid.is_not_null()) // Access via groups
                    .into_boxed();

                if !visible_only {
                    query = query.or_filter(
                        users_organizations::atype.le(MembershipType::Admin as i32) // Org admin/owner
                        );
                }

                query
                    .select(ciphers::all_columns)
                    .distinct()
                    .load::<CipherDb>(conn).expect("Error loading ciphers").from_db()
            }}
        } else {
            db_run! {conn: {
                let mut query = ciphers::table
                    .left_join(ciphers_collections::table.on(
                            ciphers::uuid.eq(ciphers_collections::cipher_uuid)
                            ))
                    .left_join(users_organizations::table.on(
                            ciphers::organization_uuid.eq(users_organizations::org_uuid.nullable())
                            .and(users_organizations::user_uuid.eq(user_uuid))
                            .and(users_organizations::status.eq(MembershipStatus::Confirmed as i32))
                            ))
                    .left_join(users_collections::table.on(
                            ciphers_collections::collection_uuid.eq(users_collections::collection_uuid)
                            // Ensure that users_collections::user_uuid is NULL for unconfirmed users.
                            .and(users_organizations::user_uuid.eq(users_collections::user_uuid))
                            ))
                    .filter(ciphers::user_uuid.eq(user_uuid)) // Cipher owner
                    .or_filter(users_organizations::access_all.eq(true)) // access_all in org
                    .or_filter(users_collections::user_uuid.eq(user_uuid)) // Access to collection
                    .into_boxed();

                    if !visible_only {
                        query = query.or_filter(
                            users_organizations::atype.le(MembershipType::Admin as i32) // Org admin/owner
                            );
                    }

                query
                    .select(ciphers::all_columns)
                    .distinct()
                    .load::<CipherDb>(conn).expect("Error loading ciphers").from_db()
            }}
        }
    }

    // Find all ciphers visible to the specified user.
    pub async fn find_by_user_visible(user_uuid: &UserId, conn: &mut DbConn) -> Vec<Self> {
        Self::find_by_user(user_uuid, true, conn).await
    }

    // Find all ciphers directly owned by the specified user.
    pub async fn find_owned_by_user(user_uuid: &UserId, conn: &mut DbConn) -> Vec<Self> {
        db_run! {conn: {
            ciphers::table
                .filter(
                    ciphers::user_uuid.eq(user_uuid)
                    .and(ciphers::organization_uuid.is_null())
                )
                .load::<CipherDb>(conn).expect("Error loading ciphers").from_db()
        }}
    }

    pub async fn count_owned_by_user(user_uuid: &UserId, conn: &mut DbConn) -> i64 {
        db_run! {conn: {
            ciphers::table
                .filter(ciphers::user_uuid.eq(user_uuid))
                .count()
                .first::<i64>(conn)
                .ok()
                .unwrap_or(0)
        }}
    }

    pub async fn find_by_org(org_uuid: &OrganizationId, conn: &mut DbConn) -> Vec<Self> {
        db_run! {conn: {
            ciphers::table
                .filter(ciphers::organization_uuid.eq(org_uuid))
                .load::<CipherDb>(conn).expect("Error loading ciphers").from_db()
        }}
    }

    pub async fn count_by_org(org_uuid: &OrganizationId, conn: &mut DbConn) -> i64 {
        db_run! {conn: {
            ciphers::table
                .filter(ciphers::organization_uuid.eq(org_uuid))
                .count()
                .first::<i64>(conn)
                .ok()
                .unwrap_or(0)
        }}
    }

    pub async fn find_by_folder(folder_uuid: &FolderId, conn: &mut DbConn) -> Vec<Self> {
        db_run! {conn: {
            folders_ciphers::table.inner_join(ciphers::table)
                .filter(folders_ciphers::folder_uuid.eq(folder_uuid))
                .select(ciphers::all_columns)
                .load::<CipherDb>(conn).expect("Error loading ciphers").from_db()
        }}
    }

    /// Find all ciphers that were deleted before the specified datetime.
    pub async fn find_deleted_before(dt: &NaiveDateTime, conn: &mut DbConn) -> Vec<Self> {
        db_run! {conn: {
            ciphers::table
                .filter(ciphers::deleted_at.lt(dt))
                .load::<CipherDb>(conn).expect("Error loading ciphers").from_db()
        }}
    }

    pub async fn get_collections(&self, user_uuid: UserId, conn: &mut DbConn) -> Vec<CollectionId> {
        if CONFIG.org_groups_enabled() {
            db_run! {conn: {
                ciphers_collections::table
                    .filter(ciphers_collections::cipher_uuid.eq(&self.uuid))
                    .inner_join(collections::table.on(
                        collections::uuid.eq(ciphers_collections::collection_uuid)
                    ))
                    .left_join(users_organizations::table.on(
                        users_organizations::org_uuid.eq(collections::org_uuid)
                        .and(users_organizations::user_uuid.eq(user_uuid.clone()))
                    ))
                    .left_join(users_collections::table.on(
                        users_collections::collection_uuid.eq(ciphers_collections::collection_uuid)
                        .and(users_collections::user_uuid.eq(user_uuid.clone()))
                    ))
                    .left_join(groups_users::table.on(
                        groups_users::users_organizations_uuid.eq(users_organizations::uuid)
                    ))
                    .left_join(groups::table.on(groups::uuid.eq(groups_users::groups_uuid)))
                    .left_join(collections_groups::table.on(
                        collections_groups::collections_uuid.eq(ciphers_collections::collection_uuid)
                        .and(collections_groups::groups_uuid.eq(groups::uuid))
                    ))
                    .filter(users_organizations::access_all.eq(true) // User has access all
                        .or(users_collections::user_uuid.eq(user_uuid) // User has access to collection
                            .and(users_collections::read_only.eq(false)))
                        .or(groups::access_all.eq(true)) // Access via groups
                        .or(collections_groups::collections_uuid.is_not_null() // Access via groups
                            .and(collections_groups::read_only.eq(false)))
                    )
                    .select(ciphers_collections::collection_uuid)
                    .load::<CollectionId>(conn).unwrap_or_default()
            }}
        } else {
            db_run! {conn: {
                ciphers_collections::table
                    .filter(ciphers_collections::cipher_uuid.eq(&self.uuid))
                    .inner_join(collections::table.on(
                        collections::uuid.eq(ciphers_collections::collection_uuid)
                    ))
                    .inner_join(users_organizations::table.on(
                        users_organizations::org_uuid.eq(collections::org_uuid)
                        .and(users_organizations::user_uuid.eq(user_uuid.clone()))
                    ))
                    .left_join(users_collections::table.on(
                        users_collections::collection_uuid.eq(ciphers_collections::collection_uuid)
                        .and(users_collections::user_uuid.eq(user_uuid.clone()))
                    ))
                    .filter(users_organizations::access_all.eq(true) // User has access all
                        .or(users_collections::user_uuid.eq(user_uuid) // User has access to collection
                            .and(users_collections::read_only.eq(false)))
                    )
                    .select(ciphers_collections::collection_uuid)
                    .load::<CollectionId>(conn).unwrap_or_default()
            }}
        }
    }

    pub async fn get_admin_collections(&self, user_uuid: UserId, conn: &mut DbConn) -> Vec<CollectionId> {
        if CONFIG.org_groups_enabled() {
            db_run! {conn: {
                ciphers_collections::table
                    .filter(ciphers_collections::cipher_uuid.eq(&self.uuid))
                    .inner_join(collections::table.on(
                        collections::uuid.eq(ciphers_collections::collection_uuid)
                    ))
                    .left_join(users_organizations::table.on(
                        users_organizations::org_uuid.eq(collections::org_uuid)
                        .and(users_organizations::user_uuid.eq(user_uuid.clone()))
                    ))
                    .left_join(users_collections::table.on(
                        users_collections::collection_uuid.eq(ciphers_collections::collection_uuid)
                        .and(users_collections::user_uuid.eq(user_uuid.clone()))
                    ))
                    .left_join(groups_users::table.on(
                        groups_users::users_organizations_uuid.eq(users_organizations::uuid)
                    ))
                    .left_join(groups::table.on(groups::uuid.eq(groups_users::groups_uuid)))
                    .left_join(collections_groups::table.on(
                        collections_groups::collections_uuid.eq(ciphers_collections::collection_uuid)
                        .and(collections_groups::groups_uuid.eq(groups::uuid))
                    ))
                    .filter(users_organizations::access_all.eq(true) // User has access all
                        .or(users_collections::user_uuid.eq(user_uuid) // User has access to collection
                            .and(users_collections::read_only.eq(false)))
                        .or(groups::access_all.eq(true)) // Access via groups
                        .or(collections_groups::collections_uuid.is_not_null() // Access via groups
                            .and(collections_groups::read_only.eq(false)))
                        .or(users_organizations::atype.le(MembershipType::Admin as i32)) // User is admin or owner
                    )
                    .select(ciphers_collections::collection_uuid)
                    .load::<CollectionId>(conn).unwrap_or_default()
            }}
        } else {
            db_run! {conn: {
                ciphers_collections::table
                    .filter(ciphers_collections::cipher_uuid.eq(&self.uuid))
                    .inner_join(collections::table.on(
                        collections::uuid.eq(ciphers_collections::collection_uuid)
                    ))
                    .inner_join(users_organizations::table.on(
                        users_organizations::org_uuid.eq(collections::org_uuid)
                        .and(users_organizations::user_uuid.eq(user_uuid.clone()))
                    ))
                    .left_join(users_collections::table.on(
                        users_collections::collection_uuid.eq(ciphers_collections::collection_uuid)
                        .and(users_collections::user_uuid.eq(user_uuid.clone()))
                    ))
                    .filter(users_organizations::access_all.eq(true) // User has access all
                        .or(users_collections::user_uuid.eq(user_uuid) // User has access to collection
                            .and(users_collections::read_only.eq(false)))
                        .or(users_organizations::atype.le(MembershipType::Admin as i32)) // User is admin or owner
                    )
                    .select(ciphers_collections::collection_uuid)
                    .load::<CollectionId>(conn).unwrap_or_default()
            }}
        }
    }

    /// Return a Vec with (cipher_uuid, collection_uuid)
    /// This is used during a full sync so we only need one query for all collections accessible.
    pub async fn get_collections_with_cipher_by_user(
        user_uuid: UserId,
        conn: &mut DbConn,
    ) -> Vec<(CipherId, CollectionId)> {
        db_run! {conn: {
            ciphers_collections::table
            .inner_join(collections::table.on(
                collections::uuid.eq(ciphers_collections::collection_uuid)
            ))
            .inner_join(users_organizations::table.on(
                users_organizations::org_uuid.eq(collections::org_uuid).and(
                    users_organizations::user_uuid.eq(user_uuid.clone())
                )
            ))
            .left_join(users_collections::table.on(
                users_collections::collection_uuid.eq(ciphers_collections::collection_uuid).and(
                    users_collections::user_uuid.eq(user_uuid.clone())
                )
            ))
            .left_join(groups_users::table.on(
                groups_users::users_organizations_uuid.eq(users_organizations::uuid)
            ))
            .left_join(groups::table.on(
                groups::uuid.eq(groups_users::groups_uuid)
            ))
            .left_join(collections_groups::table.on(
                collections_groups::collections_uuid.eq(ciphers_collections::collection_uuid).and(
                    collections_groups::groups_uuid.eq(groups::uuid)
                )
            ))
            .or_filter(users_collections::user_uuid.eq(user_uuid)) // User has access to collection
            .or_filter(users_organizations::access_all.eq(true)) // User has access all
            .or_filter(users_organizations::atype.le(MembershipType::Admin as i32)) // User is admin or owner
            .or_filter(groups::access_all.eq(true)) //Access via group
            .or_filter(collections_groups::collections_uuid.is_not_null()) //Access via group
            .select(ciphers_collections::all_columns)
            .distinct()
            .load::<(CipherId, CollectionId)>(conn).unwrap_or_default()
        }}
    }
}

#[derive(
    Clone,
    Debug,
    AsRef,
    Deref,
    DieselNewType,
    Display,
    From,
    FromForm,
    Hash,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    UuidFromParam,
)]
pub struct CipherId(String);
