import React, { useEffect, useState, useCallback, useRef } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  api, mediaUrl, pickFiles, pickFolder,
  Session, FileItem, Album, LocalAccount,
  SET_GALLERY, SET_TRASH, SET_ALBUM,
} from "./api";

type View = "gallery" | "albums" | "trash" | "settings";

function fmtMB(mb: number): string {
  if (mb >= 1024) return (mb / 1024).toFixed(1) + " GB";
  return mb + " MB";
}

function sameDay(a: Date, b: Date): boolean {
  return a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate();
}

function dateLabel(ms: number): string {
  const d = new Date(ms);
  const now = new Date();
  const yest = new Date(now);
  yest.setDate(now.getDate() - 1);
  if (sameDay(d, now)) return "Today";
  if (sameDay(d, yest)) return "Yesterday";
  return d.toLocaleDateString(undefined, { year: "numeric", month: "long", day: "numeric" });
}

type DateGroup = { label: string; entries: { f: FileItem; idx: number }[] };

/** Group a date-descending file list into per-day sections (order preserved). */
function groupByDate(items: FileItem[]): DateGroup[] {
  const groups: DateGroup[] = [];
  let cur: DateGroup | null = null;
  items.forEach((f, idx) => {
    const label = dateLabel(f.date_created);
    if (!cur || cur.label !== label) {
      cur = { label, entries: [] };
      groups.push(cur);
    }
    cur.entries.push({ f, idx });
  });
  return groups;
}

/** A selectable photo grid: click opens; checkbox/Ctrl-click toggles; Shift-click
 *  selects a range; drag a marquee box to lasso. Optionally date-grouped. */
function PhotoGrid({ items, set, albumId, grouped, sel, setSel, onOpen, renderExtra }: {
  items: FileItem[]; set: number; albumId: string | null; grouped: boolean;
  sel: Set<string>; setSel: (s: Set<string>) => void;
  onOpen: (idx: number) => void;
  renderExtra?: (f: FileItem) => React.ReactNode;
}) {
  const wrapRef = useRef<HTMLDivElement>(null);
  const anchorRef = useRef<number | null>(null);
  const drag = useRef<{ x: number; y: number; base: Set<string> } | null>(null);
  const last = useRef({ x: 0, y: 0 });
  const moved = useRef(false);
  const pendingIdx = useRef(-1);     // tile under a plain press (resolved on mouseup)
  const isMarquee = useRef(false);
  const raf = useRef<number | null>(null);
  const vel = useRef(0);
  const [box, setBox] = useState<{ l: number; t: number; w: number; h: number } | null>(null);

  // Current state reachable from the window mouseup handler (avoids stale closures).
  const stateRef = useRef({ sel, items, onOpen });
  stateRef.current = { sel, items, onOpen };

  const toggleOne = (idx: number) => {
    const fn = items[idx].filename;
    const next = new Set(sel);
    next.has(fn) ? next.delete(fn) : next.add(fn);
    setSel(next);
    anchorRef.current = idx;
  };

  useEffect(() => {
    const scroller = () => wrapRef.current?.closest(".content") as HTMLElement | null;

    const applySelection = () => {
      const d = drag.current;
      if (!d) return;
      const l = Math.min(d.x, last.current.x), t = Math.min(d.y, last.current.y);
      const r = Math.max(d.x, last.current.x), b = Math.max(d.y, last.current.y);
      setBox({ l, t, w: r - l, h: b - t });
      // Accumulate so items already swept stay selected after they scroll out
      // of the (viewport-fixed) box during auto-scroll.
      wrapRef.current?.querySelectorAll<HTMLElement>("[data-fn]").forEach((el) => {
        const rc = el.getBoundingClientRect();
        if (rc.left < r && rc.right > l && rc.top < b && rc.bottom > t) d.base.add(el.dataset.fn!);
      });
      setSel(new Set(d.base));
    };

    const tick = () => {
      if (!drag.current || vel.current === 0) { raf.current = null; return; }
      const sc = scroller();
      if (sc) sc.scrollTop += vel.current;
      applySelection();
      raf.current = requestAnimationFrame(tick);
    };

    const onMove = (e: MouseEvent) => {
      const d = drag.current;
      if (!d) return;
      last.current = { x: e.clientX, y: e.clientY };
      if (!moved.current && Math.hypot(e.clientX - d.x, e.clientY - d.y) < 6) return;
      moved.current = true; isMarquee.current = true;
      const sc = scroller();
      if (sc) {
        const rect = sc.getBoundingClientRect();
        const zone = 64;
        if (e.clientY > rect.bottom - zone) vel.current = Math.min(26, ((e.clientY - (rect.bottom - zone)) / zone) * 26);
        else if (e.clientY < rect.top + zone) vel.current = -Math.min(26, (((rect.top + zone) - e.clientY) / zone) * 26);
        else vel.current = 0;
      }
      if (vel.current !== 0 && raf.current === null) raf.current = requestAnimationFrame(tick);
      applySelection();
    };
    const onUp = () => {
      if (drag.current) {
        // A plain press with no marquee drag = a click → open or toggle.
        if (!isMarquee.current && pendingIdx.current >= 0) {
          const st = stateRef.current;
          const fn = st.items[pendingIdx.current]?.filename;
          if (fn !== undefined) {
            if (st.sel.size > 0) {
              const next = new Set(st.sel);
              next.has(fn) ? next.delete(fn) : next.add(fn);
              setSel(next);
            } else {
              st.onOpen(pendingIdx.current);
            }
            anchorRef.current = pendingIdx.current;
          }
        }
        drag.current = null;
        vel.current = 0;
        if (raf.current !== null) { cancelAnimationFrame(raf.current); raf.current = null; }
        setBox(null);
      }
      pendingIdx.current = -1;
      isMarquee.current = false;
    };
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
    return () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
      if (raf.current !== null) cancelAnimationFrame(raf.current);
    };
  }, [setSel]);

  const tileIndexFrom = (target: HTMLElement): number => {
    const el = target.closest("[data-fn]") as HTMLElement | null;
    return el ? items.findIndex((x) => x.filename === el.dataset.fn) : -1;
  };

  // All tile interaction is resolved from mousedown / mouseup — no onClick — so
  // there is no trailing click event that could double-handle a selection.
  const startDrag = (e: React.MouseEvent) => {
    if (e.button !== 0) return;
    const target = e.target as HTMLElement;
    if (target.closest(".cover-btn")) return; // let the cover button handle itself
    const idx = tileIndexFrom(target);

    // Modifiers take precedence over WHERE on the tile you click (so a Shift-
    // click that lands on the corner checkmark still does a range, not a toggle).
    if (idx >= 0 && (e.shiftKey || e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      if (e.shiftKey) {
        let anchor = anchorRef.current ?? idx;
        if (anchor < 0 || anchor >= items.length) anchor = idx; // guard stale anchor
        anchorRef.current = anchor;
        const a = Math.max(0, Math.min(anchor, idx));
        const b = Math.min(items.length - 1, Math.max(anchor, idx));
        const next = new Set(sel);
        for (let i = a; i <= b; i++) next.add(items[i].filename);
        setSel(next);
      } else {
        toggleOne(idx);
      }
      return;
    }
    // Plain click on the checkmark corner → toggle just this tile.
    if (target.closest(".check") && idx >= 0) {
      e.preventDefault();
      toggleOne(idx);
      return;
    }
    // Plain press: begin a marquee; if it doesn't move, mouseup treats it as a click.
    drag.current = { x: e.clientX, y: e.clientY, base: new Set() };
    last.current = { x: e.clientX, y: e.clientY };
    moved.current = false;
    isMarquee.current = false;
    pendingIdx.current = idx;
    if (idx >= 0) anchorRef.current = idx;
  };

  const Tile = (f: FileItem) => (
    <div key={f.filename} data-fn={f.filename}
      className={"tile" + (sel.has(f.filename) ? " sel" : "")}>
      <img loading="lazy" draggable={false} src={mediaUrl(set, f.filename, true, albumId)} />
      <div className="check">{sel.has(f.filename) ? "✓" : ""}</div>
      {sel.size === 0 && renderExtra?.(f)}
    </div>
  );

  return (
    <div ref={wrapRef} className="grid-wrap" onMouseDown={startDrag}>
      {grouped
        ? groupByDate(items).map((g) => (
            <div key={g.label}>
              <div className="date-header">{g.label}</div>
              <div className="grid">{g.entries.map(({ f }) => Tile(f))}</div>
            </div>
          ))
        : <div className="grid">{items.map((f) => Tile(f))}</div>}
      {box && <div className="marquee" style={{ left: box.l, top: box.t, width: box.w, height: box.h }} />}
    </div>
  );
}

/** Full-screen image with instant (unblurred) thumbnail, silent swap to full, and zoom/pan. */
function ZoomableImage({ thumbUrl, fullUrl }: { thumbUrl: string; fullUrl: string }) {
  const [scale, setScale] = useState(1);
  const [pos, setPos] = useState({ x: 0, y: 0 });
  const [fullLoaded, setFullLoaded] = useState(false);
  const drag = useRef<{ x: number; y: number } | null>(null);
  const stageRef = useRef<HTMLDivElement>(null);

  const clamp = (s: number) => Math.min(8, Math.max(1, s));

  useEffect(() => { setScale(1); setPos({ x: 0, y: 0 }); setFullLoaded(false); }, [fullUrl]);

  useEffect(() => {
    const el = stageRef.current;
    if (!el) return;
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      setScale((s) => {
        const ns = clamp(s * (e.deltaY < 0 ? 1.15 : 1 / 1.15));
        if (ns === 1) setPos({ x: 0, y: 0 });
        return ns;
      });
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  }, []);

  const onMouseDown = (e: React.MouseEvent) => {
    if (scale > 1) drag.current = { x: e.clientX - pos.x, y: e.clientY - pos.y };
  };
  const onMouseMove = (e: React.MouseEvent) => {
    if (drag.current) setPos({ x: e.clientX - drag.current.x, y: e.clientY - drag.current.y });
  };
  const endDrag = () => { drag.current = null; };
  const zoomIn = () => setScale((s) => clamp(s * 1.3));
  const zoomOut = () => setScale((s) => { const ns = clamp(s / 1.3); if (ns === 1) setPos({ x: 0, y: 0 }); return ns; });
  const reset = () => { setScale(1); setPos({ x: 0, y: 0 }); };

  return (
    <>
      <div
        ref={stageRef}
        className="zoom-stage"
        onClick={(e) => e.stopPropagation()}
        onMouseDown={onMouseDown}
        onMouseMove={onMouseMove}
        onMouseUp={endDrag}
        onMouseLeave={endDrag}
        onDoubleClick={() => (scale > 1 ? reset() : setScale(2))}
        style={{ cursor: scale > 1 ? (drag.current ? "grabbing" : "grab") : "default" }}
      >
        <div className="zoom-inner" style={{ transform: `translate(${pos.x}px,${pos.y}px) scale(${scale})` }}>
          <img className="base" src={thumbUrl} draggable={false} />
          <img className="over" src={fullUrl} draggable={false}
            style={{ opacity: fullLoaded ? 1 : 0 }} onLoad={() => setFullLoaded(true)} />
        </div>
      </div>
      <div className="zoom-controls" onClick={(e) => e.stopPropagation()}>
        <button onClick={zoomOut}>−</button>
        <button onClick={reset}>{Math.round(scale * 100)}%</button>
        <button onClick={zoomIn}>＋</button>
      </div>
    </>
  );
}

export default function App() {
  const [session, setSession] = useState<Session | null>(null);
  const [loading, setLoading] = useState(true);
  const [toast, setToast] = useState<string | null>(null);

  const showToast = useCallback((m: string) => {
    setToast(m);
    setTimeout(() => setToast(null), 2600);
  }, []);

  const refreshSession = useCallback(async () => {
    const s = await api.session();
    setSession(s.logged_in ? s : null);
  }, []);

  useEffect(() => {
    api.session().then((s) => {
      setSession(s.logged_in ? s : null);
      setLoading(false);
    });
  }, []);

  if (loading) return <div className="auth"><div className="spinner" /></div>;
  if (!session) return <AuthView onAuthed={setSession} />;
  return <Main session={session} setSession={setSession} refreshSession={refreshSession} showToast={showToast} toast={toast} />;
}

/* ----------------------------- Auth ----------------------------- */

function AuthView({ onAuthed }: { onAuthed: (s: Session) => void }) {
  const [mode, setMode] = useState<"login" | "register" | "recover">("login");
  const [accounts, setAccounts] = useState<LocalAccount[]>([]);
  const [server, setServer] = useState("https://api.stingle.org/");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [phrase, setPhrase] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  useEffect(() => { api.localAccounts().then(setAccounts).catch(() => {}); }, []);

  const submit = async () => {
    setBusy(true); setError("");
    try {
      const s = mode === "login" ? await api.login(server, email, password)
        : mode === "register" ? await api.register(server, email, password)
        : await api.recover(server, email, phrase.trim(), password);
      onAuthed(s);
    } catch (e: any) {
      setError(String(e));
    } finally { setBusy(false); }
  };

  const title = mode === "login" ? "Sign In" : mode === "register" ? "Create Account" : "Recover Account";

  return (
    <div className="auth">
      <div className="auth-card">
        <h1>Stingle Photos</h1>
        <div className="sub">End-to-end encrypted photo backup</div>

        {accounts.length > 0 && mode === "login" && (
          <div className="field">
            <label>Account</label>
            <select
              style={{ width: "100%", padding: 9, background: "var(--bg)", color: "var(--fg)", border: "1px solid var(--border)", borderRadius: 8 }}
              onChange={(ev) => {
                const a = accounts.find((x) => x.email === ev.target.value);
                if (a) { setEmail(a.email); setServer(a.server_url); }
              }}
              value={email}
            >
              <option value="">New / other account…</option>
              {accounts.map((a) => <option key={a.account_key} value={a.email}>{a.email}</option>)}
            </select>
          </div>
        )}

        <div className="field">
          <label>Email</label>
          <input value={email} onChange={(e) => setEmail(e.target.value)} placeholder="you@example.com" />
        </div>

        {mode === "recover" && (
          <div className="field">
            <label>Recovery phrase (24 words)</label>
            <textarea value={phrase} onChange={(e) => setPhrase(e.target.value)} rows={3}
              style={{ width: "100%", padding: 9, background: "var(--bg)", color: "var(--fg)", border: "1px solid var(--border)", borderRadius: 8, fontFamily: "monospace", resize: "vertical" }} />
          </div>
        )}

        <div className="field">
          <label>{mode === "recover" ? "New password" : "Password"}</label>
          <input type="password" value={password} onChange={(e) => setPassword(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && submit()} />
        </div>
        <details>
          <summary className="muted" style={{ fontSize: 12, cursor: "pointer" }}>Advanced</summary>
          <div className="field" style={{ marginTop: 8 }}>
            <label>Server URL</label>
            <input value={server} onChange={(e) => setServer(e.target.value)} />
          </div>
        </details>

        {error && <div className="error">{error}</div>}

        <button className="primary" style={{ width: "100%", marginTop: 10 }} disabled={busy} onClick={submit}>
          {busy ? <span className="spinner" /> : title}
        </button>

        <div className="link-row">
          <a onClick={() => { setMode(mode === "login" ? "register" : "login"); setError(""); }}>
            {mode === "login" ? "Create an account" : "I already have an account"}
          </a>
          <a onClick={() => { setMode(mode === "recover" ? "login" : "recover"); setError(""); }}>
            {mode === "recover" ? "Back to sign in" : "Recover with phrase"}
          </a>
        </div>
      </div>
    </div>
  );
}

/* ----------------------------- Main ----------------------------- */

function Main({ session, setSession, refreshSession, showToast, toast }: {
  session: Session; setSession: (s: Session | null) => void;
  refreshSession: () => Promise<void>; showToast: (m: string) => void; toast: string | null;
}) {
  const [view, setView] = useState<View>("gallery");
  const [syncing, setSyncing] = useState(false);
  const [reloadKey, setReloadKey] = useState(0);
  const [thumbs, setThumbs] = useState<{ done: number; total: number } | null>(null);
  const reload = () => setReloadKey((k) => k + 1);

  useEffect(() => {
    const u1 = listen<[number, number]>("thumbs-progress", (e) =>
      setThumbs({ done: e.payload[0], total: e.payload[1] })
    );
    const u2 = listen<number>("thumbs-done", () => {
      setThumbs(null);
      reload();
    });
    return () => { u1.then((f) => f()); u2.then((f) => f()); };
  }, []);

  const doSync = useCallback(async () => {
    setSyncing(true);
    try {
      const r = await api.sync();
      await refreshSession();
      reload();
      showToast(`Synced — ${r.gallery} photos, ${r.albums} albums`);
    } catch (e) { showToast("Sync failed: " + e); }
    finally { setSyncing(false); }
  }, [refreshSession, showToast]);

  useEffect(() => {
    const un = listen("tray-sync", () => doSync());
    doSync(); // initial sync on load
    return () => { un.then((f) => f()); };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const pct = session.space_quota > 0 ? Math.min(100, (session.space_used / session.space_quota) * 100) : 0;

  return (
    <div className="app">
      <div className="sidebar">
        <div className="brand"><span className="dot" /> Stingle</div>
        <div className="nav">
          <button className={view === "gallery" ? "active" : ""} onClick={() => setView("gallery")}>🖼️ Gallery</button>
          <button className={view === "albums" ? "active" : ""} onClick={() => setView("albums")}>📁 Albums</button>
          <button className={view === "trash" ? "active" : ""} onClick={() => setView("trash")}>🗑️ Trash</button>
          <button className={view === "settings" ? "active" : ""} onClick={() => setView("settings")}>⚙️ Settings</button>
        </div>
        <div className="spacer" />
        <div className="acct">{session.email}</div>
        <div className="storage-bar"><div style={{ width: pct + "%" }} /></div>
        <div className="acct">{fmtMB(session.space_used)} / {fmtMB(session.space_quota)}</div>
        {thumbs && thumbs.total > 0 && (
          <div className="acct">⤓ Thumbnails {thumbs.done}/{thumbs.total}</div>
        )}
      </div>

      <div className="main">
        {view === "gallery" && <GalleryView reloadSignal={reloadKey} syncing={syncing} onSync={doSync} showToast={showToast} onChanged={reload} />}
        {view === "albums" && <AlbumsView reloadSignal={reloadKey} showToast={showToast} />}
        {view === "trash" && <TrashView reloadSignal={reloadKey} showToast={showToast} onChanged={reload} />}
        {view === "settings" && <SettingsView session={session} setSession={setSession} showToast={showToast} />}
      </div>

      {toast && <div className="toast">{toast}</div>}
    </div>
  );
}

/* ----------------------------- Gallery ----------------------------- */

function GalleryView({ syncing, onSync, showToast, onChanged, reloadSignal }: {
  syncing: boolean; onSync: () => void; showToast: (m: string) => void; onChanged: () => void; reloadSignal: number;
}) {
  const [items, setItems] = useState<FileItem[]>([]);
  const [sel, setSel] = useState<Set<string>>(new Set());
  const [viewerIdx, setViewerIdx] = useState<number | null>(null);

  const load = useCallback(() => { api.listGallery(0, 100000).then(setItems); }, []);
  useEffect(() => { load(); }, [load, reloadSignal]);

  const doImport = async () => {
    const files = await pickFiles();
    if (files.length === 0) return;
    const n = await api.importPaths(files, null);
    showToast(`Imported ${n} file(s) — syncing…`);
    onSync();
  };

  const doImportFolder = async () => {
    const dir = await pickFolder();
    if (!dir) return;
    const n = await api.importPaths([dir], null);
    showToast(`Imported ${n} file(s) — syncing…`);
    onSync();
  };

  return (
    <>
      <div className="topbar">
        <h2>Gallery</h2>
        <div className="actionbar">
          {sel.size > 0 ? (
            <>
              <span className="muted">{sel.size} selected</span>
              <button onClick={() => setSel(new Set(items.map((f) => f.filename)))}>Select all</button>
              <ActionButtons set={SET_GALLERY} albumId={null} filenames={[...sel]}
                onDone={() => { setSel(new Set()); load(); onChanged(); }} showToast={showToast} />
              <button onClick={() => setSel(new Set())}>Cancel</button>
            </>
          ) : (
            <>
              <button onClick={doImport}>＋ Import Files</button>
              <button onClick={doImportFolder}>📂 Import Folder</button>
              <button className="primary" onClick={onSync} disabled={syncing}>
                {syncing ? <span className="spinner" /> : "↻ Sync"}
              </button>
            </>
          )}
        </div>
      </div>
      <div className="content">
        {items.length === 0 ? (
          <div className="empty">No photos yet.<br />Import files to get started.</div>
        ) : (
          <PhotoGrid items={items} set={SET_GALLERY} albumId={null} grouped
            sel={sel} setSel={setSel} onOpen={setViewerIdx} />
        )}
      </div>
      {viewerIdx !== null && (
        <Viewer items={items} index={viewerIdx} set={SET_GALLERY} albumId={null}
          onClose={() => setViewerIdx(null)} onChanged={() => { load(); onChanged(); }} showToast={showToast} />
      )}
    </>
  );
}

/* ----------------------------- Albums ----------------------------- */

function AlbumsView({ showToast, reloadSignal }: { showToast: (m: string) => void; reloadSignal: number }) {
  const [albums, setAlbums] = useState<Album[]>([]);
  const [open, setOpen] = useState<Album | null>(null);
  const load = useCallback(() => { api.listAlbums().then(setAlbums); }, []);
  useEffect(() => { load(); }, [load, reloadSignal]);

  const create = async () => {
    const name = prompt("Album name:");
    if (!name) return;
    await api.createAlbum(name);
    showToast("Album created");
    load();
  };

  if (open) return <AlbumDetail album={open} onBack={() => { setOpen(null); load(); }} showToast={showToast} />;

  return (
    <>
      <div className="topbar">
        <h2>Albums</h2>
        <div className="actionbar"><button className="primary" onClick={create}>＋ New Album</button></div>
      </div>
      <div className="content">
        {albums.length === 0 ? <div className="empty">No albums yet.</div> : (
          <div className="albums">
            {albums.map((a) => (
              <div key={a.album_id} className="album-card" onClick={() => setOpen(a)}>
                <div className="cover">
                  {a.cover ? <img src={mediaUrl(SET_ALBUM, a.cover, true, a.album_id)} /> : "📁"}
                </div>
                <div className="meta">
                  <div className="name">{a.name} {a.is_shared ? "👥" : ""}</div>
                  <div className="count">{a.count} item(s){a.is_owner ? "" : " · shared with you"}</div>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>
    </>
  );
}

function AlbumDetail({ album, onBack, showToast }: { album: Album; onBack: () => void; showToast: (m: string) => void }) {
  const [items, setItems] = useState<FileItem[]>([]);
  const [viewerIdx, setViewerIdx] = useState<number | null>(null);
  const [sel, setSel] = useState<Set<string>>(new Set());
  const load = useCallback(() => { api.listAlbumFiles(album.album_id).then(setItems); }, [album.album_id]);
  useEffect(() => { load(); }, [load]);

  const clearSel = () => setSel(new Set());

  const addFiles = async () => {
    const files = await pickFiles();
    if (!files.length) return;
    const n = await api.importPaths(files, album.album_id);
    showToast(`Imported ${n} — syncing…`);
    await api.sync();
    load();
  };
  const rename = async () => {
    const name = prompt("Rename album:", album.name);
    if (!name) return;
    await api.renameAlbum(album.album_id, name); showToast("Renamed");
  };
  const del = async () => {
    if (!confirm(`Delete album "${album.name}"?`)) return;
    await api.deleteAlbum(album.album_id); onBack();
  };
  const share = async () => {
    const email = prompt("Share with (email):");
    if (!email) return;
    try {
      await api.shareAlbum(album.album_id, [email.trim()], true, true, true);
      showToast("Album shared with " + email);
    } catch (e) { showToast("Share failed: " + e); }
  };
  const unshare = async () => {
    if (!confirm("Stop sharing this album?")) return;
    await api.unshareAlbum(album.album_id); showToast("Unshared");
  };
  const leave = async () => {
    if (!confirm("Leave this shared album?")) return;
    await api.leaveAlbum(album.album_id); onBack();
  };

  return (
    <>
      <div className="topbar">
        <button onClick={onBack}>←</button>
        <h2>{album.name}</h2>
        <div className="actionbar">
          {sel.size > 0 ? (
            <>
              <span className="muted">{sel.size} selected</span>
              <button onClick={() => setSel(new Set(items.map((f) => f.filename)))}>Select all</button>
              <ActionButtons set={SET_ALBUM} albumId={album.album_id} filenames={[...sel]}
                onDone={() => { clearSel(); load(); }} showToast={showToast} />
              <button onClick={clearSel}>Cancel</button>
            </>
          ) : (
            <>
              {album.is_owner && <button onClick={addFiles}>＋ Add</button>}
              {album.is_owner && <button onClick={share}>👥 Share</button>}
              {album.is_owner && album.is_shared && <button onClick={unshare}>Unshare</button>}
              {album.is_owner && <button onClick={rename}>Rename</button>}
              {album.is_owner && <button onClick={del}>Delete</button>}
              {!album.is_owner && <button onClick={leave}>Leave</button>}
            </>
          )}
        </div>
      </div>
      <div className="content">
        {items.length === 0 ? <div className="empty">Empty album.</div> : (
          <PhotoGrid items={items} set={SET_ALBUM} albumId={album.album_id} grouped={false}
            sel={sel} setSel={setSel} onOpen={setViewerIdx}
            renderExtra={(f) => album.is_owner ? (
              <button className="cover-btn" onClick={(e) => {
                e.stopPropagation();
                api.setAlbumCover(album.album_id, f.filename).then(() => showToast("Album cover set"));
              }}>★ cover</button>
            ) : null} />
        )}
      </div>
      {viewerIdx !== null && (
        <Viewer items={items} index={viewerIdx} set={SET_ALBUM} albumId={album.album_id}
          onClose={() => setViewerIdx(null)} onChanged={load} showToast={showToast} />
      )}
    </>
  );
}

/* ----------------------------- Trash ----------------------------- */

function TrashView({ showToast, onChanged, reloadSignal }: { showToast: (m: string) => void; onChanged: () => void; reloadSignal: number }) {
  const [items, setItems] = useState<FileItem[]>([]);
  const [sel, setSel] = useState<Set<string>>(new Set());
  const load = useCallback(() => { api.listTrash().then(setItems); }, []);
  useEffect(() => { load(); }, [load, reloadSignal]);

  return (
    <>
      <div className="topbar">
        <h2>Trash</h2>
        <div className="actionbar">
          {sel.size > 0 && <>
            <span className="muted">{sel.size} selected</span>
            <button onClick={() => setSel(new Set(items.map((f) => f.filename)))}>Select all</button>
            <button onClick={async () => { await api.restore([...sel]); setSel(new Set()); showToast("Restored"); load(); onChanged(); }}>Restore</button>
            <button onClick={async () => { if (confirm("Delete forever?")) { await api.deletePermanently([...sel]); setSel(new Set()); load(); } }}>Delete</button>
            <button onClick={() => setSel(new Set())}>Cancel</button>
          </>}
          {sel.size === 0 && items.length > 0 && <button onClick={async () => { if (confirm("Empty trash?")) { await api.emptyTrash(); load(); } }}>Empty Trash</button>}
        </div>
      </div>
      <div className="content">
        {items.length === 0 ? <div className="empty">Trash is empty.</div> : (
          <PhotoGrid items={items} set={SET_TRASH} albumId={null} grouped
            sel={sel} setSel={setSel}
            onOpen={(idx) => setSel(new Set([items[idx].filename]))} />
        )}
      </div>
    </>
  );
}

/* ----------------------------- Settings ----------------------------- */

function SettingsView({ session, setSession, showToast }: {
  session: Session; setSession: (s: Session | null) => void; showToast: (m: string) => void;
}) {
  const [phrase, setPhrase] = useState<string | null>(null);
  const [cacheLimit, setCacheLimit] = useState<string>("0");
  const [cacheSizeMB, setCacheSizeMB] = useState<number | null>(null);

  const refreshCache = () => { api.cacheSize().then((b) => setCacheSizeMB(b / 1048576)); };
  useEffect(() => {
    api.getCacheLimit().then((b) => setCacheLimit(String(Math.round(b / 1048576))));
    refreshCache();
  }, []);
  const saveCacheLimit = async () => {
    const mb = Math.max(0, parseInt(cacheLimit) || 0);
    await api.setCacheLimit(mb * 1048576);
    showToast(mb === 0 ? "Cache limit: unlimited" : `Cache limit set to ${mb} MB`);
    refreshCache();
  };
  const clearCacheNow = async () => { await api.clearCache(); showToast("Cache cleared"); refreshCache(); };

  const doTakeout = async () => {
    const dir = await pickFolder();
    if (!dir) return;
    showToast("Exporting… this may take a while");
    const r = await api.takeout(dir, false);
    showToast(`Takeout complete — ${r.written} files (${r.errors} errors)`);
  };

  return (
    <>
      <div className="topbar"><h2>Settings</h2></div>
      <div className="content">
        <div className="settings-section">
          <h3>Account</h3>
          <div className="muted">{session.email}</div>
          <div className="muted" style={{ fontSize: 13, marginTop: 4 }}>
            Storage: {fmtMB(session.space_used)} of {fmtMB(session.space_quota)} ·
            Recovery key {session.is_key_backed_up ? "backed up" : "not backed up"}
          </div>
        </div>

        <div className="settings-section">
          <h3>Recovery phrase</h3>
          <p className="muted" style={{ fontSize: 13 }}>
            Your recovery phrase restores your account if you forget your password. Keep it secret and safe.
          </p>
          {phrase ? <div className="phrase">{phrase}</div> :
            <button onClick={async () => setPhrase(await api.recoveryPhrase())}>Reveal recovery phrase</button>}
        </div>

        <div className="settings-section">
          <h3>Cache</h3>
          <p className="muted" style={{ fontSize: 13 }}>
            Encrypted downloads are cached on disk for fast viewing.
            {cacheSizeMB !== null && ` Currently using ${cacheSizeMB.toFixed(1)} MB.`}
            {" "}When the limit is exceeded, the oldest cached files (and their thumbnails) are removed.
          </p>
          <div className="row" style={{ maxWidth: 360, alignItems: "center" }}>
            <input type="number" min={0} value={cacheLimit} onChange={(e) => setCacheLimit(e.target.value)} />
            <span className="muted" style={{ whiteSpace: "nowrap" }}>MB (0 = unlimited)</span>
          </div>
          <div className="actionbar" style={{ marginTop: 10 }}>
            <button onClick={saveCacheLimit}>Save limit</button>
            <button onClick={clearCacheNow}>Clear cache now</button>
          </div>
        </div>

        <div className="settings-section">
          <h3>Takeout</h3>
          <p className="muted" style={{ fontSize: 13 }}>Download and decrypt your entire library to a folder.</p>
          <button onClick={doTakeout}>Export library…</button>
        </div>

        <div className="settings-section">
          <h3>Session</h3>
          <div className="actionbar">
            <button onClick={async () => { await api.lock(); setSession(null); }}>Lock</button>
            <button onClick={async () => { await api.logout(false); setSession(null); }}>Sign out</button>
          </div>
        </div>
      </div>
    </>
  );
}

/* ----------------------------- Actions ----------------------------- */

type MoveDest = { type: "gallery" } | { type: "album"; id: string };

function MoveDialog({ fromSet, fromAlbum, count, onPick, onClose }: {
  fromSet: number; fromAlbum: string | null; count: number;
  onPick: (dest: MoveDest) => void; onClose: () => void;
}) {
  const [albums, setAlbums] = useState<Album[]>([]);
  useEffect(() => {
    api.listAlbums().then(setAlbums);
    const h = (e: KeyboardEvent) => { if (e.key === "Escape") onClose(); };
    window.addEventListener("keydown", h);
    return () => window.removeEventListener("keydown", h);
  }, [onClose]);

  const targets = albums.filter((a) => a.album_id !== fromAlbum && a.is_owner);
  const priv = targets.filter((a) => !a.is_shared);
  const shared = targets.filter((a) => a.is_shared);

  const newAlbum = async () => {
    const name = prompt("New album name:");
    if (!name) return;
    const id = await api.createAlbum(name);
    onPick({ type: "album", id });
  };

  const Card = (a: Album) => (
    <div key={a.album_id} className="move-card" onClick={() => onPick({ type: "album", id: a.album_id })}>
      <div className="move-cover">
        {a.cover ? <img src={mediaUrl(SET_ALBUM, a.cover, true, a.album_id)} /> : <span>📁</span>}
      </div>
      <div className="move-name" title={a.name}>{a.name}</div>
      <div className="move-count">{a.count} item{a.count === 1 ? "" : "s"}</div>
    </div>
  );

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal move-dialog" onClick={(e) => e.stopPropagation()}>
        <div className="move-head">
          <h3>Move {count} item{count === 1 ? "" : "s"} to…</h3>
          <button className="icon-btn" onClick={onClose}>✕</button>
        </div>
        <div className="move-grid">
          {fromSet === SET_ALBUM && (
            <div className="move-card" onClick={() => onPick({ type: "gallery" })}>
              <div className="move-cover special"><span>🖼️</span></div>
              <div className="move-name">Gallery</div>
              <div className="move-count">Main library</div>
            </div>
          )}
          <div className="move-card" onClick={newAlbum}>
            <div className="move-cover special"><span>＋</span></div>
            <div className="move-name">New album</div>
            <div className="move-count">Create &amp; move</div>
          </div>
        </div>
        {priv.length > 0 && <><div className="move-section">My albums</div><div className="move-grid">{priv.map(Card)}</div></>}
        {shared.length > 0 && <><div className="move-section">Shared albums</div><div className="move-grid">{shared.map(Card)}</div></>}
      </div>
    </div>
  );
}

function ActionButtons({ set, albumId, filenames, onDone, showToast }: {
  set: number; albumId: string | null; filenames: string[];
  onDone: () => void; showToast: (m: string) => void;
}) {
  const [moving, setMoving] = useState(false);
  const save = async () => {
    const dir = await pickFolder();
    if (!dir) return;
    showToast("Saving…");
    const n = await api.saveFiles(set, albumId, filenames, dir);
    showToast(`Saved ${n} file${n === 1 ? "" : "s"}`);
  };
  const del = async () => {
    await api.trashCtx(set, albumId, filenames);
    showToast("Moved to trash");
    onDone();
  };
  const move = async (dest: MoveDest) => {
    setMoving(false);
    try {
      if (dest.type === "gallery") await api.moveToGallery(albumId!, filenames);
      else await api.moveToAlbum(set, albumId, filenames, dest.id);
      showToast("Moved");
      onDone();
    } catch (e) { showToast("Move failed: " + e); }
  };
  return (
    <>
      <button onClick={save}>⤓ Save</button>
      <button onClick={() => setMoving(true)}>→ Move</button>
      <button onClick={del}>🗑️ Delete</button>
      {moving && <MoveDialog fromSet={set} fromAlbum={albumId} count={filenames.length} onPick={move} onClose={() => setMoving(false)} />}
    </>
  );
}

/* ----------------------------- Viewer ----------------------------- */

function Viewer({ items, index, set, albumId, onClose, onChanged, showToast }: {
  items: FileItem[]; index: number; set: number; albumId: string | null;
  onClose: () => void; onChanged: () => void; showToast: (m: string) => void;
}) {
  const [i, setI] = useState(index);
  const [isVid, setIsVid] = useState<boolean | null>(null);
  const f = items[i];
  useEffect(() => {
    const h = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
      if (e.key === "ArrowRight") setI((x) => Math.min(items.length - 1, x + 1));
      if (e.key === "ArrowLeft") setI((x) => Math.max(0, x - 1));
    };
    window.addEventListener("keydown", h);
    return () => window.removeEventListener("keydown", h);
  }, [items.length, onClose]);

  useEffect(() => {
    let alive = true;
    setIsVid(null);
    if (f) api.isVideo(set, f.filename, albumId).then((v) => alive && setIsVid(v)).catch(() => alive && setIsVid(false));
    return () => { alive = false; };
  }, [i, f, set, albumId]);

  if (!f) return null;
  const thumbUrl = mediaUrl(set, f.filename, true, albumId);
  const fullUrl = mediaUrl(set, f.filename, false, albumId);
  const stop = (e: React.MouseEvent) => e.stopPropagation();
  return (
    <div className="viewer" onClick={onClose}>
      <div className="close">✕</div>
      <div className="viewer-actions" onClick={stop}>
        <ActionButtons set={set} albumId={albumId} filenames={[f.filename]}
          onDone={() => { onChanged(); onClose(); }} showToast={showToast} />
      </div>
      {i > 0 && <button className="nav-btn prev" onClick={(e) => { e.stopPropagation(); setI(i - 1); }}>‹</button>}
      {isVid === null ? <div className="spinner" />
        : isVid ? <video src={fullUrl} controls autoPlay onClick={stop} />
        : <ZoomableImage key={f.filename} thumbUrl={thumbUrl} fullUrl={fullUrl} />}
      {i < items.length - 1 && <button className="nav-btn next" onClick={(e) => { e.stopPropagation(); setI(i + 1); }}>›</button>}
      <div className="name">{i + 1} / {items.length}</div>
    </div>
  );
}
