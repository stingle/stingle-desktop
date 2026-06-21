import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";

export type Session = {
  logged_in: boolean;
  email: string | null;
  user_id: string | null;
  server_url: string | null;
  space_used: number;
  space_quota: number;
  is_key_backed_up: boolean;
};

export type FileItem = {
  filename: string;
  album_id: string | null;
  date_created: number;
  date_modified: number;
  is_local: boolean;
  is_remote: boolean;
  is_video: boolean;
};

export type Album = {
  album_id: string;
  name: string;
  is_owner: boolean;
  is_shared: boolean;
  cover: string;
  count: number;
};

export type LocalAccount = {
  account_key: string;
  email: string;
  server_url: string;
};

export const SET_GALLERY = 0;
export const SET_TRASH = 1;
export const SET_ALBUM = 2;

export const api = {
  localAccounts: () => invoke<LocalAccount[]>("list_local_accounts"),
  session: () => invoke<Session>("session"),
  register: (serverUrl: string, email: string, password: string, isBackup = true) =>
    invoke<Session>("register", { serverUrl, email, password, isBackup }),
  login: (serverUrl: string, email: string, password: string) =>
    invoke<Session>("login", { serverUrl, email, password }),
  resume: (accountKey: string, password: string) =>
    invoke<Session>("resume", { accountKey, password }),
  lock: () => invoke("lock"),
  logout: (wipe: boolean) => invoke("logout", { wipe }),
  sync: () => invoke<{ gallery: number; trash: number; albums: number }>("sync"),
  listGallery: (offset: number, limit: number) =>
    invoke<FileItem[]>("list_gallery", { offset, limit }),
  listTrash: () => invoke<FileItem[]>("list_trash"),
  listAlbums: () => invoke<Album[]>("list_albums"),
  listAlbumFiles: (albumId: string) =>
    invoke<FileItem[]>("list_album_files", { albumId }),
  importPaths: (paths: string[], albumId: string | null) =>
    invoke<number>("import_paths", { paths, albumId }),
  trash: (filenames: string[]) => invoke("trash", { filenames }),
  restore: (filenames: string[]) => invoke("restore", { filenames }),
  deletePermanently: (filenames: string[]) =>
    invoke("delete_permanently", { filenames }),
  emptyTrash: () => invoke("empty_trash"),
  createAlbum: (name: string) => invoke<string>("create_album", { name }),
  renameAlbum: (albumId: string, name: string) =>
    invoke("rename_album", { albumId, name }),
  deleteAlbum: (albumId: string) => invoke("delete_album", { albumId }),
  setAlbumCover: (albumId: string, filename: string) =>
    invoke("set_album_cover", { albumId, filename }),
  takeout: (outDir: string, includeTrash: boolean) =>
    invoke<{ written: number; errors: number }>("takeout", { outDir, includeTrash }),
  recoveryPhrase: () => invoke<string>("recovery_phrase"),
  isVideo: (set: number, filename: string, albumId: string | null) =>
    invoke<boolean>("is_video", { set, albumId, filename }),
  recover: (serverUrl: string, email: string, mnemonic: string, newPassword: string) =>
    invoke<Session>("recover", { serverUrl, email, mnemonic, newPassword }),
  shareAlbum: (
    albumId: string, emails: string[],
    allowAdd: boolean, allowShare: boolean, allowCopy: boolean
  ) => invoke("share_album", { albumId, emails, allowAdd, allowShare, allowCopy }),
  unshareAlbum: (albumId: string) => invoke("unshare_album", { albumId }),
  leaveAlbum: (albumId: string) => invoke("leave_album", { albumId }),
  // Cache management
  getCacheLimit: () => invoke<number>("get_cache_limit"),
  setCacheLimit: (bytes: number) => invoke("set_cache_limit", { bytes }),
  cacheSize: () => invoke<number>("cache_size"),
  clearCache: () => invoke("clear_cache"),
  // File actions
  saveFiles: (set: number, albumId: string | null, filenames: string[], destDir: string) =>
    invoke<number>("save_files", { set, albumId, filenames, destDir }),
  moveToAlbum: (set: number, albumId: string | null, filenames: string[], toAlbum: string, isMoving: boolean) =>
    invoke("move_to_album", { set, albumId, filenames, toAlbum, isMoving }),
  moveToGallery: (albumId: string, filenames: string[], isMoving: boolean) =>
    invoke("move_to_gallery", { albumId, filenames, isMoving }),
  trashCtx: (set: number, albumId: string | null, filenames: string[]) =>
    invoke("trash_ctx", { set, albumId, filenames }),
  // Login screen / returning user
  lastAccount: () => invoke<LocalAccount | null>("last_account"),
  forgetAccount: (accountKey: string) =>
    invoke("forget_account", { accountKey }),
  // Auto-unlock
  secureStoreStatus: () => invoke<{ biometric: boolean }>("secure_store_status"),
  isAutoUnlockEnabled: () => invoke<boolean>("is_auto_unlock_enabled"),
  enableAutoUnlock: (password: string, allowPlaintext: boolean) =>
    invoke<{ used_plaintext: boolean }>("enable_auto_unlock", { password, allowPlaintext }),
  disableAutoUnlock: () => invoke("disable_auto_unlock"),
  tryAutoUnlock: () => invoke<Session>("try_auto_unlock"),
  // App options
  getAutostart: () => invoke<boolean>("get_autostart"),
  setAutostart: (enabled: boolean) => invoke("set_autostart", { enabled }),
  getMinimizeToTray: () => invoke<boolean>("get_minimize_to_tray"),
  setMinimizeToTray: (enabled: boolean) => invoke("set_minimize_to_tray", { enabled }),
  getSyncEverything: () => invoke<boolean>("get_sync_everything"),
  setSyncEverything: (enabled: boolean) => invoke("set_sync_everything", { enabled }),
  getStoragePath: () => invoke<string>("get_storage_path"),
  changeStoragePath: (newPath: string) => invoke("change_storage_path", { newPath }),
  // Clipboard
  copyFilesToClipboard: (set: number, albumId: string | null, filenames: string[]) =>
    invoke<number>("copy_files_to_clipboard", { set, albumId, filenames }),
  clipboardFiles: () => invoke<string[]>("clipboard_files"),
  pasteFromClipboard: (albumId: string | null) =>
    invoke<number>("paste_from_clipboard", { albumId }),
  // Drag-out (decrypts to a temp file for the OS drag, cleaned up after)
  exportForDrag: (set: number, albumId: string | null, filenames: string[]) =>
    invoke<{ files: string[]; icon: string }>("export_for_drag", { set, albumId, filenames }),
  cleanupDragExport: (paths: string[]) => invoke("cleanup_drag_export", { paths }),
};

/** URL for a decrypted thumbnail/original served via the `stingle://` protocol. */
export function mediaUrl(
  set: number,
  filename: string,
  isThumb: boolean,
  albumId?: string | null
): string {
  const a = albumId && albumId.length ? albumId : "-";
  // `!` delimiter: never appears in base64 and is left untouched by
  // encodeURIComponent, so filenames/album-ids containing / + = parse cleanly.
  return convertFileSrc(`${set}!${isThumb ? 1 : 0}!${a}!${filename}`, "stingle");
}

export async function pickFiles(): Promise<string[]> {
  const res = await open({
    multiple: true,
    directory: false,
    filters: [
      {
        name: "Media",
        extensions: [
          "jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff", "heic",
          "mp4", "mov", "avi", "mkv", "webm", "m4v", "3gp",
        ],
      },
    ],
  });
  if (!res) return [];
  return Array.isArray(res) ? res : [res];
}

export async function pickFolder(): Promise<string | null> {
  const res = await open({ directory: true, multiple: false });
  return (res as string) || null;
}
