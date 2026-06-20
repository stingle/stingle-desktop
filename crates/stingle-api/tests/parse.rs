//! Offline tests for response-envelope and model parsing.

use serde_json::json;
use stingle_api::models::{DeleteEvent, RemoteAlbum, RemoteContact, RemoteFile};
use stingle_api::response::StingleResponse;

#[test]
fn envelope_status_logout_and_arrays() {
    // `files` provided as a real array; numbers as JSON numbers.
    let v = json!({
        "status": "ok",
        "parts": {
            "files": [
                {"file":"a.sp","version":1,"headers":"H1","dateCreated":1000,"dateModified":2000},
                {"file":"b.sp","albumId":"","version":2,"headers":"H2","dateCreated":3,"dateModified":4}
            ],
            "spaceUsed": "12345",
            "spaceQuota": 999
        },
        "infos": ["hi"],
        "errors": []
    });
    let r = StingleResponse::from_value(v);
    assert!(r.is_ok());
    assert!(!r.logged_out());
    let files: Vec<RemoteFile> = r.parse_array("files");
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].filename, "a.sp");
    assert_eq!(files[0].date_modified, 2000);
    assert_eq!(r.get("spaceUsed").as_deref(), Some("12345"));
    assert_eq!(r.get("spaceQuota").as_deref(), Some("999"));
}

#[test]
fn array_encoded_as_string_is_parsed() {
    // Some deployments return the set as a JSON-encoded string.
    let files_str = r#"[{"file":"x.sp","version":"3","headers":"H","dateCreated":"10","dateModified":"20"}]"#;
    let v = json!({ "status":"ok", "parts": { "files": files_str } });
    let r = StingleResponse::from_value(v);
    let files: Vec<RemoteFile> = r.parse_array("files");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].version, Some(3)); // numeric string accepted
    assert_eq!(files[0].date_created, 10);
}

#[test]
fn logout_flag_detected() {
    let v = json!({ "status":"ok", "parts": { "logout": "1" } });
    let r = StingleResponse::from_value(v);
    assert!(r.logged_out());
    assert!(r.into_result().is_err());
}

#[test]
fn album_and_contact_and_delete_event_parse() {
    let album: RemoteAlbum = serde_json::from_value(json!({
        "albumId":"AID","encPrivateKey":"ESK","publicKey":"PK","metadata":"M",
        "isShared":1,"isHidden":0,"isOwner":"1","permissions":"1111","isLocked":0,
        "cover":"c.sp","members":"5,6","dateCreated":1,"dateModified":2
    }))
    .unwrap();
    assert!(album.is_shared && album.is_owner && !album.is_hidden);
    assert_eq!(album.members, "5,6");

    let contact: RemoteContact = serde_json::from_value(json!({
        "userId": 42, "email":"a@b.c", "publicKey":"CPK"
    }))
    .unwrap();
    assert_eq!(contact.user_id, 42);
    assert_eq!(contact.date_used, None);

    let del: DeleteEvent = serde_json::from_value(json!({
        "file":"f.sp","albumId":"","type":3,"date":777
    }))
    .unwrap();
    assert_eq!(del.event_type, 3);
    assert_eq!(del.date, 777);
}
