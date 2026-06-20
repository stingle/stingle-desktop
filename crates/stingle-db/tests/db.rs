use stingle_db::{Db, DbAlbum, DbContact, DbFile, FileSet, Sort};

fn sample_file(name: &str, created: i64) -> DbFile {
    DbFile {
        id: 0,
        album_id: None,
        filename: name.to_string(),
        is_local: true,
        is_remote: false,
        version: 1,
        reupload: false,
        date_created: created,
        date_modified: created,
        headers: "H".to_string(),
    }
}

#[test]
fn gallery_crud_and_listing() {
    let db = Db::open_in_memory().unwrap();
    db.insert_file(FileSet::Gallery, &sample_file("a.sp", 100)).unwrap();
    db.insert_file(FileSet::Gallery, &sample_file("b.sp", 300)).unwrap();
    db.insert_file(FileSet::Gallery, &sample_file("c.sp", 200)).unwrap();

    assert_eq!(db.count_files(FileSet::Gallery).unwrap(), 3);

    // Sorted by date_created DESC.
    let list = db.list_files(FileSet::Gallery, Sort::Desc, None, 0).unwrap();
    assert_eq!(
        list.iter().map(|f| f.filename.as_str()).collect::<Vec<_>>(),
        vec!["b.sp", "c.sp", "a.sp"]
    );

    // only_local before upload, then mark_remote moves it out.
    assert_eq!(db.list_only_local(FileSet::Gallery, Sort::Desc).unwrap().len(), 3);
    db.mark_remote(FileSet::Gallery, "a.sp").unwrap();
    let a = db.get_file(FileSet::Gallery, "a.sp").unwrap().unwrap();
    assert!(a.is_remote && !a.reupload);
    assert_eq!(db.list_only_local(FileSet::Gallery, Sort::Desc).unwrap().len(), 2);

    // reupload flag
    db.set_reupload(FileSet::Gallery, "b.sp", true).unwrap();
    assert_eq!(db.list_reupload(FileSet::Gallery).unwrap().len(), 1);

    db.delete_file(FileSet::Gallery, "a.sp").unwrap();
    assert_eq!(db.count_files(FileSet::Gallery).unwrap(), 2);

    let dates = db.distinct_dates(FileSet::Gallery).unwrap();
    assert_eq!(dates, vec![300, 200]);
}

#[test]
fn album_files_and_albums() {
    let db = Db::open_in_memory().unwrap();
    let album = DbAlbum {
        album_id: "AID".into(),
        enc_private_key: "ESK".into(),
        public_key: "PK".into(),
        metadata: "M".into(),
        is_shared: true,
        is_hidden: false,
        is_owner: true,
        members: "1,2".into(),
        permissions: "1111".into(),
        sync_local: false,
        is_locked: false,
        cover: "".into(),
        date_created: 1,
        date_modified: 2,
    };
    db.upsert_album(&album).unwrap();
    // sync_local preserved across an update that doesn't set it.
    db.set_album_sync_local("AID", true).unwrap();
    let mut updated = album.clone();
    updated.date_modified = 5;
    db.upsert_album(&updated).unwrap();
    let got = db.get_album("AID").unwrap().unwrap();
    assert!(got.sync_local, "sync_local must survive a server-driven update");
    assert_eq!(got.date_modified, 5);

    let mut af = sample_file("x.sp", 10);
    af.album_id = Some("AID".into());
    db.insert_album_file(&af).unwrap();
    assert_eq!(db.count_album_files("AID").unwrap(), 1);
    assert!(db.get_album_file("AID", "x.sp").unwrap().is_some());
    db.mark_album_file_remote("AID", "x.sp").unwrap();
    assert!(db.get_album_file("AID", "x.sp").unwrap().unwrap().is_remote);

    db.delete_all_files_in_album("AID").unwrap();
    assert_eq!(db.count_album_files("AID").unwrap(), 0);

    assert_eq!(db.list_albums(false).unwrap().len(), 1);
    db.delete_album("AID").unwrap();
    assert_eq!(db.list_albums(true).unwrap().len(), 0);
}

#[test]
fn contacts_imported_and_kv() {
    let db = Db::open_in_memory().unwrap();
    db.upsert_contact(&DbContact {
        user_id: 7,
        email: "a@b.c".into(),
        public_key: "PK".into(),
        date_used: 0,
        date_modified: 1,
    })
    .unwrap();
    assert_eq!(db.get_contact_by_email("a@b.c").unwrap().unwrap().user_id, 7);
    assert!(db.get_contact_by_user_id(7).unwrap().is_some());
    db.delete_contact(7).unwrap();
    assert!(db.list_contacts().unwrap().is_empty());

    assert!(!db.is_imported("path-hash").unwrap());
    db.mark_imported("path-hash").unwrap();
    assert!(db.is_imported("path-hash").unwrap());

    db.kv_set_i64("filesST", 12345).unwrap();
    assert_eq!(db.kv_get_i64("filesST").unwrap(), Some(12345));
    assert_eq!(db.kv_get("missing").unwrap(), None);
}
