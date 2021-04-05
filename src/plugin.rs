use age_core::format::{FileKey, Stanza};
use age_plugin::{
    identity::{self, IdentityPluginV1},
    recipient::{self, RecipientPluginV1},
    Callbacks,
};
use bech32::{FromBase32, Variant};
use std::collections::HashMap;
use std::io;

use crate::{format, p256::Recipient, yubikey, IDENTITY_PREFIX, RECIPIENT_PREFIX};

#[derive(Debug, Default)]
pub(crate) struct RecipientPlugin {
    recipients: Vec<Recipient>,
    yubikeys: Vec<yubikey::Stub>,
}

impl RecipientPluginV1 for RecipientPlugin {
    fn add_recipients<'a, I: Iterator<Item = &'a str>>(
        &mut self,
        recipients: I,
    ) -> Result<(), Vec<recipient::Error>> {
        let errors: Vec<_> = recipients
            .enumerate()
            .filter_map(|(index, recipient)| {
                if let Some(pk) = bech32::decode(recipient)
                    .ok()
                    .and_then(|(hrp, data, variant)| {
                        if hrp == RECIPIENT_PREFIX && variant == Variant::Bech32 {
                            Some(data)
                        } else {
                            None
                        }
                    })
                    .and_then(|data| Vec::from_base32(&data).ok())
                    .and_then(|bytes| Recipient::from_bytes(&bytes))
                {
                    self.recipients.push(pk);
                    None
                } else {
                    Some(recipient::Error::Recipient {
                        index,
                        message: "Invalid recipient".to_owned(),
                    })
                }
            })
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    fn add_identities<'a, I: Iterator<Item = &'a str>>(
        &mut self,
        identities: I,
    ) -> Result<(), Vec<recipient::Error>> {
        let errors: Vec<_> = identities
            .enumerate()
            .filter_map(|(index, identity)| {
                if let Some(stub) = bech32::decode(identity)
                    .ok()
                    .and_then(|(hrp, data, variant)| {
                        if hrp == IDENTITY_PREFIX.to_lowercase() && variant == Variant::Bech32 {
                            Some(data)
                        } else {
                            None
                        }
                    })
                    .and_then(|data| Vec::from_base32(&data).ok())
                    .and_then(|bytes| yubikey::Stub::from_bytes(&bytes, index))
                {
                    self.yubikeys.push(stub);
                    None
                } else {
                    Some(recipient::Error::Identity {
                        index,
                        message: "Invalid Yubikey stub".to_owned(),
                    })
                }
            })
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    fn wrap_file_keys(
        &mut self,
        file_keys: Vec<FileKey>,
        mut callbacks: impl Callbacks<recipient::Error>,
    ) -> io::Result<Result<Vec<Vec<Stanza>>, Vec<recipient::Error>>> {
        // Connect to any listed YubiKey identities to obtain the corresponding recipients.
        let mut yk_recipients = vec![];
        let mut yk_errors = vec![];
        for stub in &self.yubikeys {
            match stub.connect(&mut callbacks)? {
                Ok(conn) => yk_recipients.push(conn.recipient().clone()),
                Err(e) => yk_errors.push(match e {
                    identity::Error::Identity { index, message } => {
                        recipient::Error::Identity { index, message }
                    }
                    // stub.connect() only returns identity::Error::Identity
                    _ => unreachable!(),
                }),
            }
        }

        // If any errors occurred while fetching recipients from YubiKeys, don't encrypt
        // the file to any of the other recipients.
        Ok(if yk_errors.is_empty() {
            Ok(file_keys
                .into_iter()
                .map(|file_key| {
                    self.recipients
                        .iter()
                        .chain(yk_recipients.iter())
                        .map(|pk| format::RecipientLine::wrap_file_key(&file_key, &pk).into())
                        .collect()
                })
                .collect())
        } else {
            Err(yk_errors)
        })
    }
}

#[derive(Debug, Default)]
pub(crate) struct IdentityPlugin {
    yubikeys: Vec<yubikey::Stub>,
}

impl IdentityPluginV1 for IdentityPlugin {
    fn add_identities<'a, I: Iterator<Item = &'a str>>(
        &mut self,
        identities: I,
    ) -> Result<(), Vec<identity::Error>> {
        let errors: Vec<_> = identities
            .enumerate()
            .filter_map(|(index, identity)| {
                if let Some(stub) = bech32::decode(identity)
                    .ok()
                    .and_then(|(hrp, data, variant)| {
                        if hrp == IDENTITY_PREFIX.to_lowercase() && variant == Variant::Bech32 {
                            Some(data)
                        } else {
                            None
                        }
                    })
                    .and_then(|data| Vec::from_base32(&data).ok())
                    .and_then(|bytes| yubikey::Stub::from_bytes(&bytes, index))
                {
                    self.yubikeys.push(stub);
                    None
                } else {
                    Some(identity::Error::Identity {
                        index,
                        message: "Invalid Yubikey stub".to_owned(),
                    })
                }
            })
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    fn unwrap_file_keys(
        &mut self,
        files: Vec<Vec<Stanza>>,
        mut callbacks: impl Callbacks<identity::Error>,
    ) -> io::Result<HashMap<usize, Result<FileKey, Vec<identity::Error>>>> {
        let mut file_keys = HashMap::with_capacity(files.len());

        // Filter to files / stanzas for which we have matching YubiKeys
        let mut candidate_stanzas: Vec<(
            &yubikey::Stub,
            HashMap<usize, Vec<format::RecipientLine>>,
        )> = self
            .yubikeys
            .iter()
            .map(|stub| (stub, HashMap::new()))
            .collect();

        for (file, stanzas) in files.iter().enumerate() {
            for (stanza_index, stanza) in stanzas.iter().enumerate() {
                match (
                    format::RecipientLine::from_stanza(&stanza).map(|res| {
                        res.map_err(|_| identity::Error::Stanza {
                            file_index: file,
                            stanza_index,
                            message: "Invalid yubikey stanza".to_owned(),
                        })
                    }),
                    file_keys.contains_key(&file),
                ) {
                    // Only record candidate stanzas for files without structural errors.
                    (Some(Ok(line)), false) => {
                        // A line will match at most one YubiKey.
                        if let Some(files) =
                            candidate_stanzas.iter_mut().find_map(|(stub, files)| {
                                if stub.matches(&line) {
                                    Some(files)
                                } else {
                                    None
                                }
                            })
                        {
                            files.entry(file).or_default().push(line);
                        }
                    }
                    (Some(Err(e)), _) => {
                        // This is a structurally-invalid stanza, so we MUST return errors
                        // and MUST NOT unwrap any stanzas in the same file. Let's collect
                        // these errors to return to the client.
                        match file_keys.entry(file).or_insert_with(|| Err(vec![])) {
                            Err(errors) => errors.push(e),
                            Ok(_) => unreachable!(),
                        }
                        // Drop any existing candidate stanzas from this file.
                        for (_, candidates) in candidate_stanzas.iter_mut() {
                            candidates.remove(&file);
                        }
                    }
                    _ => (),
                }
            }
        }

        // Sort by effectiveness (YubiKey that can trial-decrypt the most stanzas)
        candidate_stanzas.sort_by_key(|(_, files)| {
            files
                .iter()
                .map(|(_, stanzas)| stanzas.len())
                .sum::<usize>()
        });
        candidate_stanzas.reverse();
        // Remove any YubiKeys without stanzas.
        candidate_stanzas.retain(|(_, files)| {
            files
                .iter()
                .map(|(_, stanzas)| stanzas.len())
                .sum::<usize>()
                > 0
        });

        for (stub, files) in candidate_stanzas.iter() {
            let mut conn = match stub.connect(&mut callbacks)? {
                Ok(conn) => conn,
                Err(e) => {
                    callbacks.error(e)?.unwrap();
                    continue;
                }
            };

            for (&file_index, stanzas) in files {
                if file_keys.contains_key(&file_index) {
                    // We decrypted this file with an earlier YubiKey.
                    continue;
                }

                for (stanza_index, line) in stanzas.iter().enumerate() {
                    match conn.unwrap_file_key(&line) {
                        Ok(file_key) => {
                            // We've managed to decrypt this file!
                            file_keys.entry(file_index).or_insert(Ok(file_key));
                            break;
                        }
                        Err(_) => callbacks
                            .error(identity::Error::Stanza {
                                file_index,
                                stanza_index,
                                message: "Failed to decrypt YubiKey stanza".to_owned(),
                            })?
                            .unwrap(),
                    }
                }
            }
        }
        Ok(file_keys)
    }
}