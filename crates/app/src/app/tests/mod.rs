//! The app's tests, by subject; `support` holds the helpers they share.

use super::*;
use egui::{Key, Modifiers};
use egui_phosphor::regular as icon;
use rongo_core::history::{HistoryKey, Kind, Store};
use rongo_core::profile::{ConnectionProfile, SecretKind};
use rongo_core::vault::vault_key;
use rongo_core::bson::{Bson, Document, doc};
use rongo_core::{CollKind, Command, ConnectRequest, CopyReport, CoreError, Event, FindSpec, Namespace, Response, CopiedItem};
use crate::indexes::IndexAction;
use crate::dialogs::{ConnectAction, Pending};
use crate::net::{HistoryReq, Origin, Sent, ViewReq};
use crate::paste::{PasteAction, TargetInfo};
use crate::result_view::{Mode, ViewAction};
use crate::sidebar::{CollNode, Status, TreeAction};
use crate::settings::{HistorySettings, OpenIn, SettingsAction, mb};
use crate::tools::{HistoryOffer, ToolKind};
use crate::tabs::CollectionTab;
use crate::unlock::UnlockResult;
use super::history::{Unrecorded, WITHOUT_HISTORY};
use super::outcome::{copy_failed, copy_summary, copy_troubled, move_failed, outcome_text, restored_toast};
use super::status::OpKey;
use super::vault::DELETE_ALL_HISTORY;
use support::*;

mod history;
mod history_tab;
mod paste;
mod settings;
mod status;
mod support;
mod tree;
mod ui;
mod update;
mod vault;
