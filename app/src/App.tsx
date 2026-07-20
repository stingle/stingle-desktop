import React, { useEffect, useLayoutEffect, useState, useCallback, useRef, useMemo } from "react";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { startDrag } from "@crabnebula/tauri-plugin-drag";
import {
  api, mediaUrl, videoUrl, pickFiles, pickFolder, parsePermissions,
  Session, FileItem, Album, SharedAlbum, Contact, AlbumMember, LocalAccount, WatchFolder,
  SET_GALLERY, SET_TRASH, SET_ALBUM, BLANK_COVER,
} from "./api";
import logoUrl from "./assets/stingle-logo.png";

type View = "gallery" | "albums" | "sharing" | "contacts" | "trash" | "settings";

/* ----------------------------- Icons ----------------------------- */
// Crisp inline stroke icons (currentColor) replacing the generic emoji.
const ICON_PROPS = {
  width: 18, height: 18, viewBox: "0 0 24 24", fill: "none",
  stroke: "currentColor", strokeWidth: 1.7,
  strokeLinecap: "round" as const, strokeLinejoin: "round" as const,
};
const GalleryIcon = () => (
  <svg {...ICON_PROPS}>
    <rect x="3" y="3" width="18" height="18" rx="2.5" />
    <circle cx="8.5" cy="8.5" r="1.6" />
    <path d="M21 15l-5-5L5 21" />
  </svg>
);
const AlbumsIcon = () => (
  <svg {...ICON_PROPS}>
    <path d="M3 7.5A1.5 1.5 0 0 1 4.5 6h4l2 2.2h8.5A1.5 1.5 0 0 1 20.5 9.7" />
    <rect x="3" y="8.2" width="18" height="11" rx="1.8" />
  </svg>
);
const TrashIcon = () => (
  <svg {...ICON_PROPS}>
    <path d="M4 7h16" />
    <path d="M9 7V5.2A1.2 1.2 0 0 1 10.2 4h3.6A1.2 1.2 0 0 1 15 5.2V7" />
    <path d="M6 7l1 12.2A1.8 1.8 0 0 0 8.8 21h6.4a1.8 1.8 0 0 0 1.8-1.8L18 7" />
    <path d="M10 11v6M14 11v6" />
  </svg>
);
const SharingIcon = () => (
  <svg {...ICON_PROPS}>
    <circle cx="18" cy="5" r="2.6" />
    <circle cx="6" cy="12" r="2.6" />
    <circle cx="18" cy="19" r="2.6" />
    <path d="M8.3 10.7l7.4-4.3M8.3 13.3l7.4 4.3" />
  </svg>
);
const ContactsIcon = () => (
  <svg {...ICON_PROPS}>
    <circle cx="9" cy="8" r="3.2" />
    <path d="M3.5 20a5.5 5.5 0 0 1 11 0" />
    <path d="M16 5.2a3.2 3.2 0 0 1 0 6" />
    <path d="M17.5 14.3A5.5 5.5 0 0 1 20.5 19" />
  </svg>
);
const SettingsIcon = () => (
  <svg {...ICON_PROPS}>
    <circle cx="12" cy="12" r="3.1" />
    <path d="M19.4 13a1.6 1.6 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.6 1.6 0 0 0-2.7 1.1V19a2 2 0 1 1-4 0v-.1a1.6 1.6 0 0 0-2.7-1.1l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1A1.6 1.6 0 0 0 4.6 13a2 2 0 1 1 0-4 1.6 1.6 0 0 0 1.1-2.7l-.1-.1A2 2 0 1 1 8.4 3.4l.1.1A1.6 1.6 0 0 0 11 4.6 2 2 0 1 1 15 4.6a1.6 1.6 0 0 0 2.7 1.1l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1A1.6 1.6 0 0 0 19.4 11a2 2 0 1 1 0 4z" />
  </svg>
);

function fmtMB(mb: number): string {
  // Never render a negative size — a bad/absent server value must read as 0.
  const v = Math.max(0, mb);
  if (v >= 1024) return (v / 1024).toFixed(1) + " GB";
  return v + " MB";
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

/** Group a date-descending file list into per-day sections (order preserved).
 *  Day boundaries are detected with a cheap numeric key; the human label
 *  (which goes through Intl and costs ~40µs a call) is formatted once per
 *  GROUP, not per item — at 25k items that's the difference between ~1s and
 *  a few ms per (re)grouping. */
function groupByDate(items: FileItem[]): DateGroup[] {
  const groups: DateGroup[] = [];
  let cur: DateGroup | null = null;
  let curKey = -1;
  items.forEach((f, idx) => {
    const d = new Date(f.date_created);
    const key = d.getFullYear() * 10000 + d.getMonth() * 100 + d.getDate();
    if (!cur || key !== curKey) {
      curKey = key;
      cur = { label: dateLabel(f.date_created), entries: [] };
      groups.push(cur);
    }
    cur.entries.push({ f, idx });
  });
  return groups;
}

/** True while a native drag-OUT (library → other app) is in flight. If the user
 *  releases the drag back over our OWN window ("let it go too soon"), Tauri
 *  delivers a webview "drop" whose paths are the decrypted temp export files; the
 *  drag-drop handler checks this flag to avoid re-importing them. Kept set until
 *  shortly after the drag ends, because the drop event and the drag-end callback
 *  resolve at roughly the same instant and the drop may arrive last. */
let dragOutActive = false;
let dragOutToken = 0;

/** Start a native OS drag of one or more library items out to other apps
 *  (Explorer, Telegram, …). Files are decrypted to a temp folder for the drag
 *  and cleaned up when it ends. Returns immediately if the export fails. */
async function nativeDragOut(set: number, albumId: string | null, filenames: string[]) {
  if (filenames.length === 0) return;
  dragOutActive = true;
  const token = ++dragOutToken;
  try {
    const exp = await api.exportForDrag(set, albumId, filenames);
    const cleanup = exp.icon ? [...exp.files, exp.icon] : exp.files;
    await startDrag(
      { item: exp.files, icon: exp.icon, mode: "copy" },
      () => { api.cleanupDragExport(cleanup); }
    );
  } catch { /* drag export failed — ignore */ }
  finally {
    // Clear only after the trailing drop/leave event has reached the drag-drop
    // listener. The token guard means only the most recent drag clears the flag,
    // so back-to-back drags never expose a gap.
    setTimeout(() => { if (token === dragOutToken) dragOutActive = false; }, 400);
  }
}

/** Copy library items to the OS clipboard as real files (Explorer-style), so a
 *  multi-select copy pastes all of them into Telegram/Explorer/etc. */
async function copyToClipboard(
  set: number, albumId: string | null, filenames: string[],
  showToast?: (m: string) => void
) {
  if (filenames.length === 0) return;
  try {
    const n = await api.copyFilesToClipboard(set, albumId, filenames);
    showToast?.(`Copied ${n} item${n === 1 ? "" : "s"} to clipboard`);
  } catch (e) {
    showToast?.("Copy failed: " + e);
  }
}

/** True if a keyboard event targets a text input (so we don't hijack copy/paste). */
function inTextField(e: KeyboardEvent): boolean {
  const t = e.target as HTMLElement | null;
  return !!t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable);
}

/** One thumbnail tile. Memoized so a selection change (or a scroll-driven
 *  window shift) only re-renders the tiles that actually changed. The grid is
 *  VIRTUALIZED — only tiles near the viewport exist in the DOM. The `<img>` is
 *  mounted only when `load` is true: during a fast scroll/fling the window
 *  slides over hundreds of tiles per second, and mounting each one's <img>
 *  would fire a stingle:// request that the backend decrypts to completion even
 *  after the tile scrolls away — starving the thumbnails you actually land on.
 *  `w` is the computed square tile size (a number so memoization survives). */
const TileView = React.memo(function TileView({
  f, idx, set, albumId, w, load, selected, selectionEmpty, renderExtra, onLoaded,
}: {
  f: FileItem; idx: number; set: number; albumId: string | null; w: number;
  load: boolean; selected: boolean; selectionEmpty: boolean;
  renderExtra?: (f: FileItem) => React.ReactNode;
  onLoaded: (filename: string) => void;
}) {
  return (
    <div data-fn={f.filename} data-idx={idx} className={"tile" + (selected ? " sel" : "")}
      style={{ width: w, height: w }}>
      {load && <img draggable={false} src={mediaUrl(set, f.filename, true, albumId)}
        onLoad={() => onLoaded(f.filename)} />}
      {f.is_video && <div className="vid-badge" aria-label="Video"><svg viewBox="0 0 24 24" width="13" height="13" fill="currentColor"><path d="M8 5v14l11-7z" /></svg></div>}
      {f.is_local && !f.is_remote && (
        <div className="cloud-badge" aria-label="Not uploaded yet" title="Not uploaded yet">
          <svg viewBox="0 0 24 24" width="14" height="14" fill="currentColor"><path d="M19.35 10.04C18.67 6.59 15.64 4 12 4c-1.48 0-2.85.43-4.01 1.17l1.46 1.46C10.21 6.23 11.08 6 12 6c3.04 0 5.5 2.46 5.5 5.5v.5H19c1.66 0 3 1.34 3 3 0 1.13-.64 2.11-1.56 2.62l1.45 1.45C24.16 18.16 24 16 24 16c0-2.64-2.05-4.78-4.65-4.96zM3 5.27l2.75 2.74C2.56 8.15 0 10.77 0 14c0 3.31 2.69 6 6 6h11.73l2 2L21 20.73 4.27 4 3 5.27zM7.73 10l8 8H6c-2.21 0-4-1.79-4-4s1.79-4 4-4h1.73z" /></svg>
        </div>
      )}
      <div className="check">{selected ? "✓" : ""}</div>
      {selectionEmpty && renderExtra?.(f)}
    </div>
  );
});

/* ------------------------- grid virtualization -------------------------
 * The browser must never hold tens of thousands of tiles in the DOM: even
 * fully memoized and content-visibility-locked, a full-viewport overlay
 * mount/unmount forces an O(N) layout walk (~300 ms at 25k tiles) and every
 * rendered frame pays an O(N) IntersectionObserver pass (~50 ms). Windowing
 * rows keeps all of that O(visible). */
const GRID_GAP = 8;
const MIN_TILE = 148; // matches the old `minmax(148px, 1fr)` column sizing
const OVERSCAN_PX = 600; // band above/below the viewport kept mounted
const HEADER_H = 46; // date header row: 18px gap above + label + 9px below
const HEADER_H_FIRST = 28; // first header has no gap above
// A per-frame scroll jump larger than this (≈2.5 tile rows) means the user is
// flinging or dragging the scrollbar, not reading — so hold off mounting new
// thumbnails until it settles. Deliberate wheel scrolling stays under this and
// keeps loading live.
const FLING_PX = 400;
const SETTLE_MS = 120; // resume loading this long after the last fast frame

type GridRow =
  | { kind: "header"; label: string; top: number; h: number }
  | { kind: "tiles"; entries: { f: FileItem; idx: number }[]; top: number; h: number };

/** A selectable photo grid: click opens; checkbox/Ctrl-click toggles; Shift-click
 *  selects a range; drag a thumbnail to drag the file(s) out; drag from empty
 *  space to lasso a marquee selection. Optionally date-grouped. */
// Memoized: opening/closing the viewer (and other parent state changes) must NOT
// re-render the whole grid — reconciling tens of thousands of tiles makes the
// viewer take a beat to appear/close. With stable props (state setters, a
// useCallback'd showToast/renderExtra) this re-renders only when items/sel/grouped
// actually change.
const PhotoGrid = React.memo(function PhotoGrid({ items, set, albumId, grouped, sel, setSel, onOpen, renderExtra, showToast }: {
  items: FileItem[]; set: number; albumId: string | null; grouped: boolean;
  sel: Set<string>; setSel: (s: Set<string>) => void;
  onOpen: (idx: number) => void;
  renderExtra?: (f: FileItem) => React.ReactNode;
  showToast?: (m: string) => void;
}) {
  const wrapRef = useRef<HTMLDivElement>(null);
  const anchorRef = useRef<number | null>(null);
  const drag = useRef<{ x: number; y: number; base: Set<string> } | null>(null);
  const last = useRef({ x: 0, y: 0 });
  const moved = useRef(false);
  // A plain press on a thumbnail: resolves to a click (open/toggle) on mouseup,
  // or to a native file drag once the pointer moves past the threshold.
  const tilePress = useRef<{ x: number; y: number; idx: number; started: boolean } | null>(null);
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
      // Explorer-style: a press that started on a thumbnail turns into a native
      // file drag once the pointer moves past the threshold (never a marquee).
      const tp = tilePress.current;
      if (tp) {
        if (!tp.started && Math.hypot(e.clientX - tp.x, e.clientY - tp.y) >= 6) {
          tp.started = true;
          tilePress.current = null;
          const st = stateRef.current;
          const fn = st.items[tp.idx]?.filename;
          if (fn !== undefined) {
            // Drag the whole selection if this tile is part of it; otherwise drag
            // just this tile WITHOUT selecting it. Dragging a photo out to another
            // app must never disturb the selection — selection changes only via the
            // checkbox / Ctrl / Shift / marquee.
            const files = st.sel.has(fn) && st.sel.size > 0 ? [...st.sel] : [fn];
            nativeDragOut(set, albumId, files);
          }
        }
        return;
      }
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
      // A press on a thumbnail that never moved = a click → open or toggle.
      const tp = tilePress.current;
      if (tp && !tp.started) {
        const st = stateRef.current;
        const fn = st.items[tp.idx]?.filename;
        if (fn !== undefined) {
          if (st.sel.size > 0) {
            const next = new Set(st.sel);
            next.has(fn) ? next.delete(fn) : next.add(fn);
            setSel(next);
          } else {
            st.onOpen(tp.idx);
          }
          anchorRef.current = tp.idx;
        }
      }
      tilePress.current = null;
      if (drag.current) {
        drag.current = null;
        vel.current = 0;
        if (raf.current !== null) { cancelAnimationFrame(raf.current); raf.current = null; }
        setBox(null);
      }
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

  // Ctrl/Cmd+C copies the current selection to the clipboard as files.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.ctrlKey || e.metaKey) || e.key.toLowerCase() !== "c") return;
      if (inTextField(e)) return;
      const cur = stateRef.current;
      if (cur.sel.size === 0) return;
      copyToClipboard(set, albumId, [...cur.sel], showToast);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [set, albumId, showToast]);

  const tileIndexFrom = (target: HTMLElement): number => {
    const el = target.closest("[data-idx]") as HTMLElement | null;
    return el ? Number(el.dataset.idx) : -1;
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
    if (idx >= 0) {
      // Press on a thumbnail: a click, or (once it moves) a native file drag.
      // Marquee is deliberately NOT started here — only from empty space.
      tilePress.current = { x: e.clientX, y: e.clientY, idx, started: false };
      anchorRef.current = idx;
    } else {
      // Press on empty space: begin a marquee; if it doesn't move it's a no-op.
      drag.current = { x: e.clientX, y: e.clientY, base: new Set() };
      last.current = { x: e.clientX, y: e.clientY };
      moved.current = false;
      isMarquee.current = false;
    }
  };

  const selectionEmpty = sel.size === 0;

  // Thumbnails that have finished loading at least once this mount. Kept in a
  // ref (survives tile unmount/remount) so scrolling back over seen photos shows
  // them instantly and without a grey flash, even mid-fling — those are cheap
  // in-memory cache hits on the backend, not part of the decrypt backlog.
  const loadedRef = useRef<Set<string>>(new Set());
  const markLoaded = useCallback((filename: string) => { loadedRef.current.add(filename); }, []);

  // ---- virtualization: track the scroller's viewport + our width ----
  const [vp, setVp] = useState({ w: 0, h: 0, top: 0 });
  // True while the user is flinging/dragging fast; new thumbnails hold off until
  // it clears (see FLING_PX / SETTLE_MS). Not folded into `vp` so a settle-only
  // change doesn't recompute the window geometry.
  const [flinging, setFlinging] = useState(false);
  useLayoutEffect(() => {
    const wrap = wrapRef.current;
    const sc = wrap?.closest(".content") as HTMLElement | null;
    if (!wrap || !sc) return;
    let raf = 0;
    let lastTop = sc.scrollTop;
    let settle: ReturnType<typeof setTimeout> | undefined;
    const measure = () => {
      const top = sc.scrollTop;
      // A big per-frame jump = fling/scrollbar-drag → defer loads until it stops.
      if (Math.abs(top - lastTop) > FLING_PX) {
        setFlinging(true);
        if (settle) clearTimeout(settle);
        settle = setTimeout(() => setFlinging(false), SETTLE_MS);
      }
      lastTop = top;
      setVp((v) => {
        const next = { w: wrap.clientWidth, h: sc.clientHeight, top };
        return v.w === next.w && v.h === next.h && v.top === next.top ? v : next;
      });
    };
    // Coalesce scroll events to one state update per frame.
    const onScroll = () => {
      if (raf) return;
      raf = requestAnimationFrame(() => { raf = 0; measure(); });
    };
    measure(); // before first paint, so the initial window is correct
    sc.addEventListener("scroll", onScroll, { passive: true });
    const ro = new ResizeObserver(measure);
    ro.observe(sc);
    ro.observe(wrap);
    return () => {
      sc.removeEventListener("scroll", onScroll);
      ro.disconnect();
      if (raf) cancelAnimationFrame(raf);
      if (settle) clearTimeout(settle);
    };
  }, []);

  // Same column math as the old `repeat(auto-fill, minmax(148px, 1fr))`.
  const cols = Math.max(1, Math.floor((vp.w + GRID_GAP) / (MIN_TILE + GRID_GAP)));
  const tileW = vp.w > 0 ? (vp.w - (cols - 1) * GRID_GAP) / cols : MIN_TILE;
  const rowH = tileW + GRID_GAP;

  // Grouping only depends on the list; a resize must not pay for re-grouping.
  const groups = useMemo<DateGroup[]>(
    () =>
      grouped
        ? groupByDate(items)
        : [{ label: "", entries: items.map((f, idx) => ({ f, idx })) }],
    [items, grouped],
  );

  // Flatten the groups into fixed-height rows with precomputed offsets.
  // Rebuilt only when the list or the geometry changes — never on scroll or
  // selection.
  const { rows, total } = useMemo(() => {
    const out: GridRow[] = [];
    let y = 0;
    for (const g of groups) {
      if (g.label) {
        const h = y === 0 ? HEADER_H_FIRST : HEADER_H;
        out.push({ kind: "header", label: g.label, top: y, h });
        y += h;
      }
      for (let i = 0; i < g.entries.length; i += cols) {
        out.push({ kind: "tiles", entries: g.entries.slice(i, i + cols), top: y, h: rowH });
        y += rowH;
      }
    }
    return { rows: out, total: y };
  }, [groups, cols, rowH]);

  // The mounted slice: rows intersecting viewport ± overscan (binary search).
  const [start, end] = useMemo(() => {
    if (rows.length === 0) return [0, 0];
    const lo = vp.top - OVERSCAN_PX;
    const hi = vp.top + vp.h + OVERSCAN_PX;
    let a = 0, b = rows.length - 1, first = rows.length;
    while (a <= b) {
      const m = (a + b) >> 1;
      if (rows[m].top + rows[m].h > lo) { first = m; b = m - 1; } else a = m + 1;
    }
    let c = first, d = rows.length - 1, last = first - 1;
    while (c <= d) {
      const m = (c + d) >> 1;
      if (rows[m].top < hi) { last = m; c = m + 1; } else d = m - 1;
    }
    return [first, last + 1];
  }, [rows, vp.top, vp.h]);

  return (
    <div ref={wrapRef} className="grid-wrap" onMouseDown={startDrag} style={{ height: total }}>
      {rows.slice(start, end).map((row) =>
        row.kind === "header" ? (
          <div key={"h:" + row.label} className="date-header"
            style={{ top: row.top, height: row.h }}>
            {row.label}
          </div>
        ) : (
          <div key={row.entries[0].f.filename} className="vrow"
            style={{ top: row.top, height: row.h }}>
            {row.entries.map(({ f, idx }) => (
              // Load now unless we're flinging past — but always keep a thumbnail
              // we've already shown (cheap cache hit, avoids a grey flash).
              <TileView key={f.filename} f={f} idx={idx} set={set} albumId={albumId} w={tileW}
                load={!flinging || loadedRef.current.has(f.filename)}
                selected={sel.has(f.filename)} selectionEmpty={selectionEmpty}
                renderExtra={renderExtra} onLoaded={markLoaded} />
            ))}
          </div>
        )
      )}
      {box && <div className="marquee" style={{ left: box.l, top: box.t, width: box.w, height: box.h }} />}
    </div>
  );
});

/** Full-screen image with instant (unblurred) thumbnail, silent swap to full, and zoom/pan. */
function ZoomableImage({ thumbUrl, fullUrl, onDragOut }: { thumbUrl: string; fullUrl: string; onDragOut?: () => void }) {
  const [scale, setScale] = useState(1);
  const [pos, setPos] = useState({ x: 0, y: 0 });
  const [fullLoaded, setFullLoaded] = useState(false);
  // Drives the on-screen LAYOUT (fit box). Taken from whichever layer loads first —
  // normally the instant cached thumbnail — and then kept STABLE. We must not adopt
  // the full image's reported size here: an original with EXIF orientation reports
  // swapped naturalWidth/Height vs. the pre-oriented thumbnail, so overwriting this
  // when the full arrives would visibly resize the picture mid-view.
  const [natural, setNatural] = useState<{ w: number; h: number } | null>(null);
  // Best available resolution, only for the raster sharpness multiplier (R) — the
  // full image when it arrives. Updating it does NOT change the on-screen size.
  const [hiRes, setHiRes] = useState<{ w: number; h: number } | null>(null);
  const drag = useRef<{ x: number; y: number } | null>(null);
  // Press tracked when NOT zoomed → a small move drags the file out (Explorer-style).
  const press = useRef<{ x: number; y: number } | null>(null);
  const stageRef = useRef<HTMLDivElement>(null);

  const clamp = (s: number) => Math.min(8, Math.max(1, s));

  useEffect(() => { setScale(1); setPos({ x: 0, y: 0 }); setFullLoaded(false); setNatural(null); setHiRes(null); }, [fullUrl]);

  const onBaseLoad = (e: React.SyntheticEvent<HTMLImageElement>) => {
    const img = e.currentTarget;
    if (img.naturalWidth > 0) {
      // The thumbnail is pre-oriented (EXIF baked in at generation), so ITS aspect
      // is the true display aspect. Always trust it for the layout box — overwrite
      // even if the full image got here first, since the full can report raw,
      // EXIF-unaware (e.g. landscape) dimensions that would size the box wrong.
      setNatural({ w: img.naturalWidth, h: img.naturalHeight });
      setHiRes((r) => r ?? { w: img.naturalWidth, h: img.naturalHeight });
    }
  };
  const onOverLoad = (e: React.SyntheticEvent<HTMLImageElement>) => {
    const img = e.currentTarget;
    if (img.naturalWidth > 0) {
      setNatural((n) => n ?? { w: img.naturalWidth, h: img.naturalHeight }); // fallback box only until the thumb loads
      setHiRes({ w: img.naturalWidth, h: img.naturalHeight }); // full resolution → sharp zoom, no size change
    }
    setFullLoaded(true);
  };

  // The on-screen fit box (size at zoom = 1). Driven ONLY by `natural` (the
  // thumbnail's aspect), so it is fixed the moment the thumbnail loads and never
  // changes when the full image arrives — that's what stops the picture resizing.
  const box = useMemo(() => {
    if (!natural) return null;
    const fitR = Math.min((window.innerWidth * 0.92) / natural.w, (window.innerHeight * 0.88) / natural.h);
    return { w: Math.round(natural.w * fitR), h: Math.round(natural.h * fitR) };
  }, [natural]);

  // Raster multiplier for the FULL-image layer only. It's laid out at box × overR
  // and counter-scaled by 1/overR, so it occupies exactly the box but is decoded at
  // higher resolution → sharp zoom. Because it only affects that layer's internal
  // raster (not the box), updating it when the full image loads changes nothing
  // on-screen. Bounded by the image's native pixels and the webview's decode limit.
  const overR = useMemo(() => {
    if (!box || !hiRes) return 1;
    const dpr = window.devicePixelRatio || 1;
    const MAX_DEVICE_EDGE = 4096; // safe single-image decode size for the webview
    const longEdge = Math.max(hiRes.w, hiRes.h);
    const nativePerFit = longEdge / Math.max(box.w, box.h);
    return Math.max(1, Math.min(8, nativePerFit, MAX_DEVICE_EDGE / (dpr * Math.max(box.w, box.h))));
  }, [box, hiRes]);

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
    if (e.button !== 0) return;
    if (scale > 1) drag.current = { x: e.clientX - pos.x, y: e.clientY - pos.y };
    else press.current = { x: e.clientX, y: e.clientY };
  };
  const onMouseMove = (e: React.MouseEvent) => {
    if (drag.current) { setPos({ x: e.clientX - drag.current.x, y: e.clientY - drag.current.y }); return; }
    const p = press.current;
    if (p && onDragOut && Math.hypot(e.clientX - p.x, e.clientY - p.y) >= 6) {
      press.current = null;
      onDragOut();
    }
  };
  const endDrag = () => { drag.current = null; press.current = null; };
  const zoomIn = () => setScale((s) => clamp(s * 1.3));
  const zoomOut = () => setScale((s) => { const ns = clamp(s / 1.3); if (ns === 1) setPos({ x: 0, y: 0 }); return ns; });
  const reset = () => { setScale(1); setPos({ x: 0, y: 0 }); };

  return (
    <>
      <div
        ref={stageRef}
        className="zoom-stage"
        // Clicks on the actual image stay open; clicks on the empty margin
        // around it bubble up to the viewer's onClose.
        onClick={(e) => { if (e.target !== e.currentTarget) e.stopPropagation(); }}
        onMouseDown={onMouseDown}
        onMouseMove={onMouseMove}
        onMouseUp={endDrag}
        onMouseLeave={endDrag}
        onDoubleClick={() => (scale > 1 ? reset() : setScale(2))}
        style={{ cursor: scale > 1 ? (drag.current ? "grabbing" : "grab") : "default" }}
      >
        <div
          className="zoom-inner"
          style={
            box
              ? { width: box.w, height: box.h, transform: `translate(${pos.x}px,${pos.y}px) scale(${scale})` }
              : { transform: `translate(${pos.x}px,${pos.y}px)` }
          }
        >
          <img className="base" src={thumbUrl} draggable={false} onLoad={onBaseLoad} />
          <img className="over" src={fullUrl} draggable={false} onLoad={onOverLoad}
            style={box
              ? { opacity: fullLoaded ? 1 : 0, width: box.w * overR, height: box.h * overR, transform: `scale(${1 / overR})`, transformOrigin: "top left" }
              : { opacity: fullLoaded ? 1 : 0 }} />
        </div>
      </div>
      <div className="zoom-controls" onClick={(e) => e.stopPropagation()}>
        <button onClick={zoomOut}>−</button>
        <button onClick={reset}>{Math.round(scale * 100)}%</button>
        <button onClick={zoomIn}>＋</button>
      </div>
      {/* While the high-res original is still downloading/decrypting (only the
          thumbnail is showing), spin in the corner so the soft image reads as
          "loading", not "final". */}
      {!fullLoaded && <div className="full-spinner"><div className="spinner" /></div>}
    </>
  );
}

// Guards the one-time startup init. React StrictMode double-invokes effects in
// dev, which would otherwise trigger the auto-unlock biometric prompt twice.
let didInit = false;

export default function App() {
  const [session, setSession] = useState<Session | null>(null);
  const [loading, setLoading] = useState(true);
  const [toast, setToast] = useState<string | null>(null);
  // Set when the server rejected our token. Forces the unlock screen to do a
  // full online login (a plain offline resume would reuse the dead token).
  const [sessionExpired, setSessionExpired] = useState(false);

  const showToast = useCallback((m: string) => {
    setToast(m);
    setTimeout(() => setToast(null), 2600);
  }, []);

  // The backend emits this when any sync hits a server "logout" (token expired).
  useEffect(() => {
    const un = listen("session-expired", () => {
      setSession(null);
      setSessionExpired(true);
    });
    return () => { un.then((f) => f()); };
  }, []);

  const refreshSession = useCallback(async () => {
    const s = await api.session();
    setSession(s.logged_in ? s : null);
  }, []);

  useEffect(() => {
    if (didInit) return;
    didInit = true;
    // Safety net: never strand the user on the boot spinner if a startup command
    // is slow or hangs (e.g. a backend mid-restart, DB lock, or an unanswered
    // biometric prompt). After this, we drop to the login/unlock screen, from
    // which they can proceed manually.
    const safety = setTimeout(() => setLoading(false), 10000);
    (async () => {
      try {
        const s = await api.session();
        if (s.logged_in) { setSession(s); return; }
        // Not logged in: if auto-unlock is armed, try it (may prompt biometric).
        if (await api.isAutoUnlockEnabled().catch(() => false)) {
          try {
            const u = await api.tryAutoUnlock();
            if (u.logged_in) { setSession(u); return; }
          } catch { /* fall through to the login screen */ }
        }
      } catch { /* ignore */ }
      finally { clearTimeout(safety); setLoading(false); }
    })();
  }, []);

  if (loading) return <div className="auth"><div className="spinner" /></div>;
  if (!session) return (
    <AuthView
      onAuthed={(s) => { setSessionExpired(false); setSession(s); }}
      sessionExpired={sessionExpired}
    />
  );
  return <Main session={session} setSession={setSession} refreshSession={refreshSession} showToast={showToast} toast={toast} />;
}

/* ----------------------------- Auth ----------------------------- */

function AuthView({ onAuthed, sessionExpired }: { onAuthed: (s: Session) => void; sessionExpired?: boolean }) {
  const [mode, setMode] = useState<"login" | "register" | "recover">("login");
  const [lastAcc, setLastAcc] = useState<LocalAccount | null>(null);
  const [useOther, setUseOther] = useState(false);
  const [ready, setReady] = useState(false);
  const [server, setServer] = useState("https://api.stingle.org/");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [phrase, setPhrase] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  // Whether secure-store auto-unlock is set up, so we can offer a manual
  // retry. The startup auto-unlock attempt prompts once (Hello / Touch ID, or
  // the keyring-unlock dialog on Linux); if the user cancels it (or it
  // otherwise fails) this button is the only way back in short of restarting.
  const [quickUnlock, setQuickUnlock] = useState<"biometric" | "keyring" | null>(null);

  useEffect(() => {
    api.lastAccount().then((a) => { setLastAcc(a); setReady(true); }).catch(() => setReady(true));
  }, []);

  useEffect(() => {
    (async () => {
      try {
        const [enabled, store] = await Promise.all([
          api.isAutoUnlockEnabled().catch(() => false),
          api.secureStoreStatus().catch(() => ({ biometric: false, keyring: false })),
        ]);
        setQuickUnlock(enabled ? (store.biometric ? "biometric" : store.keyring ? "keyring" : null) : null);
      } catch { /* leave the button hidden */ }
    })();
  }, []);

  // Retry secure-store unlock from the login screen (re-triggers any OS
  // prompt). Only meaningful for an offline resume — a server-expired token
  // can't be revived this way, so the button is hidden when sessionExpired.
  const quickUnlockNow = async () => {
    setBusy(true); setError("");
    try {
      const u = await api.tryAutoUnlock();
      if (u.logged_in) { onAuthed(u); return; }
      setError("Unlock was canceled.");
    } catch (e: any) {
      setError(String(e));
    } finally { setBusy(false); }
  };

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

  // Returning user: unlock the last account. Try offline resume first, then a
  // full online login if that fails (e.g. token/password rotated elsewhere).
  const unlock = async () => {
    if (!lastAcc) return;
    setBusy(true); setError("");
    try {
      let s: Session;
      if (sessionExpired) {
        // The stored token was rejected by the server; an offline resume would
        // just reuse it, so go straight to a full online login for a fresh token.
        s = await api.login(lastAcc.server_url, lastAcc.email, password);
      } else {
        try { s = await api.resume(lastAcc.account_key, password); }
        catch { s = await api.login(lastAcc.server_url, lastAcc.email, password); }
      }
      onAuthed(s);
    } catch (e: any) {
      setError(String(e));
    } finally { setBusy(false); }
  };

  const title = mode === "login" ? "Sign In" : mode === "register" ? "Create Account" : "Recover Account";

  if (!ready) return <div className="auth"><div className="spinner" /></div>;

  // Returning-user view: just a password field for the last account.
  if (lastAcc && !useOther && mode === "login") {
    return (
      <div className="auth">
        <div className="auth-card">
          <h1>Stingle Desktop</h1>
          <div className="sub">Welcome back</div>
          {sessionExpired && (
            <div className="error">Your session expired. Please enter your password to sign in again.</div>
          )}
          <div className="field">
            <label>Account</label>
            <div className="muted" style={{ fontSize: 15 }}>{lastAcc.email}</div>
          </div>
          <div className="field">
            <label>Password</label>
            <input type="password" autoFocus value={password} onChange={(e) => setPassword(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && unlock()} />
          </div>
          {error && <div className="error">{error}</div>}
          <button className="primary" style={{ width: "100%", marginTop: 10 }} disabled={busy} onClick={unlock}>
            {busy ? <span className="spinner" /> : "Unlock"}
          </button>
          {quickUnlock && !sessionExpired && (
            <button style={{ width: "100%", marginTop: 8 }} disabled={busy} onClick={quickUnlockNow}>
              {quickUnlock === "biometric" ? "Unlock with biometrics" : "Unlock from system keyring"}
            </button>
          )}
          <div className="link-row" style={{ justifyContent: "center" }}>
            <a onClick={() => { setUseOther(true); setEmail(""); setPassword(""); setError(""); }}>Sign in to a different account</a>
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="auth">
      <div className="auth-card">
        <h1>Stingle Desktop</h1>
        <div className="sub">End-to-end encrypted backup</div>

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
        {lastAcc && useOther && mode === "login" && (
          <div className="link-row" style={{ justifyContent: "center" }}>
            <a onClick={() => { setUseOther(false); setError(""); }}>← Back to {lastAcc.email}</a>
          </div>
        )}
      </div>
    </div>
  );
}

/* ----------------------------- Sync panel ----------------------------- */

type Progress = { done: number; total: number } | null;

// One phase row: an icon, a label, a done/total count, a progress bar, and an
// optional ✕ button to cancel the operation behind it.
function PhaseRow({ icon, label, value, onCancel }: {
  icon: string; label: string; value: { done: number; total: number }; onCancel?: () => void;
}) {
  const total = Math.max(0, value.total);
  const done = Math.max(0, Math.min(value.done, total));
  const pct = total > 0 ? Math.max(0, Math.min(100, (done / total) * 100)) : 0;
  return (
    <div className="phase-row">
      <span className="phase-icon">{icon}</span>
      <span className="phase-label">{label}</span>
      <span className="phase-count">{done} / {total}</span>
      {onCancel && (
        <button className="phase-cancel" onClick={onCancel} title={`Cancel ${label.toLowerCase()}`}>✕</button>
      )}
      <div className="sync-bar"><div style={{ width: pct + "%" }} /></div>
    </div>
  );
}

// Consolidated sync status: a calm "Up to date" when idle, or an animated header
// plus one progress row per active phase (upload / thumbnails / cache).
function SyncPanel({ syncing, upload, thumbs, originals, importing, takeout, onCancelImport, onCancelTakeout }: {
  syncing: boolean; upload: Progress; thumbs: Progress; originals: Progress;
  importing: Progress; takeout: Progress;
  onCancelImport: () => void; onCancelTakeout: () => void;
}) {
  const up = upload && upload.total > 0 ? upload : null;
  const th = thumbs && thumbs.total > 0 ? thumbs : null;
  const or = originals && originals.total > 0 ? originals : null;
  const im = importing && importing.total > 0 ? importing : null;
  const tk = takeout && takeout.total > 0 ? takeout : null;
  const active = syncing || !!up || !!th || !!or || !!im || !!tk;

  return (
    <div className="sync-panel">
      <div className="sync-head">
        {active ? (
          <><span className="spinner" /> <span>Working…</span></>
        ) : (
          <span className="sync-idle">✓ Up to date</span>
        )}
      </div>
      {im && <PhaseRow icon="＋" label="Importing" value={im} onCancel={onCancelImport} />}
      {up && <PhaseRow icon="↑" label="Uploading" value={up} />}
      {th && <PhaseRow icon="⤓" label="Thumbnails" value={th} />}
      {or && <PhaseRow icon="⤓" label="Cache" value={or} />}
      {tk && <PhaseRow icon="↧" label="Takeout" value={tk} onCancel={onCancelTakeout} />}
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
  const [upload, setUpload] = useState<{ done: number; total: number } | null>(null);
  const [thumbs, setThumbs] = useState<{ done: number; total: number } | null>(null);
  const [originals, setOriginals] = useState<{ done: number; total: number } | null>(null);
  const [importing, setImporting] = useState<{ done: number; total: number } | null>(null);
  const [takeoutProg, setTakeoutProg] = useState<{ done: number; total: number } | null>(null);
  // Version of an available update (set regardless of the auto-update setting,
  // so the sidebar always offers a one-click restart-and-apply).
  const [updateVer, setUpdateVer] = useState<string | null>(null);
  const [updating, setUpdating] = useState(false);
  const [appVer, setAppVer] = useState<string>("");
  useEffect(() => { api.getAppVersion().then(setAppVer).catch(() => {}); }, []);
  const reload = () => setReloadKey((k) => k + 1);

  const installUpdate = useCallback(async () => {
    setUpdating(true);
    try { await api.installUpdate(); } // installs + restarts; never returns
    catch (err) { setUpdating(false); showToast("Update failed: " + err); }
  }, [showToast]);

  // The backend checks for updates at startup and every 30 min; when a newer
  // version exists it stores the version and emits `update-available`, so the
  // sidebar can offer a one-click restart-and-apply. Because the startup check
  // can finish *before* this component (and its listener) has mounted — the
  // check races login/unlock — the event alone would be missed. So we also poll
  // the stored version once on mount: whichever of the two lands first wins.
  useEffect(() => {
    api.pendingUpdate().then((v) => { if (v) setUpdateVer(v); }).catch(() => {});
    const un = listen<string>("update-available", (e) => setUpdateVer(e.payload));
    return () => { un.then((f) => f()); };
  }, []);

  // Folder-watch auto-import runs in a background loop and emits one event per
  // imported file. Unlike manual import (drag/paste/button) it has no direct
  // call into the UI, so refresh the grid here — debounced, since a batch can
  // fire many events in quick succession — and surface any import failures.
  useEffect(() => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    const scheduleReload = () => {
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => reload(), 600);
    };
    const u1 = listen<string>("watch-import-progress", () => scheduleReload());
    const u2 = listen<string>("watch-import-error", (e) => showToast(e.payload));
    return () => {
      if (timer) clearTimeout(timer);
      u1.then((f) => f());
      u2.then((f) => f());
    };
  }, [showToast]);

  useEffect(() => {
    // A *-done event can fire repeatedly as concurrent batches finish; each reload
    // rebuilds the whole items array (and re-attaches the grid observer), so coalesce
    // a burst into a single reload instead of thrashing the grid mid-scroll.
    let reloadTimer: ReturnType<typeof setTimeout> | undefined;
    const scheduleReload = () => {
      if (reloadTimer) clearTimeout(reloadTimer);
      reloadTimer = setTimeout(() => reload(), 600);
    };
    // Progress events are emitted from many concurrent download tasks, so they
    // can arrive out of order (e.g. 48 then 32). Clamp `done` to never go
    // backwards within a batch so the counter rises smoothly instead of jumping.
    const merge = (prev: { done: number; total: number } | null, done: number, total: number) => {
      const sameBatch = prev && prev.total === total;
      return { done: sameBatch ? Math.max(prev!.done, done) : done, total };
    };
    const u0 = listen<[number, number]>("upload-progress", (e) =>
      // total 0 → nothing to upload; keep the row hidden instead of showing 0/0.
      setUpload((prev) => (e.payload[1] > 0 ? merge(prev, e.payload[0], e.payload[1]) : null))
    );
    const u0b = listen("upload-done", () => {
      setUpload(null);
      scheduleReload(); // uploaded files are now remote → refresh their state badges
    });
    const u1 = listen<[number, number]>("thumbs-progress", (e) =>
      setThumbs((prev) => merge(prev, e.payload[0], e.payload[1]))
    );
    const u2 = listen<number>("thumbs-done", () => {
      setThumbs(null);
      scheduleReload(); // new thumbnails arrived → re-mount the <img>s so they show
    });
    const u3 = listen<[number, number]>("originals-progress", (e) =>
      setOriginals((prev) => (e.payload[1] > 0 ? merge(prev, e.payload[0], e.payload[1]) : null))
    );
    const u4 = listen<number>("originals-done", () => {
      setOriginals(null);
      // The background "sync everything" loop runs a full_sync (which can pull in
      // new photos) before each originals pass, so refresh the grid here too.
      scheduleReload();
    });
    const u5 = listen<[number, number]>("import-progress", (e) =>
      setImporting((prev) => (e.payload[1] > 0 ? merge(prev, e.payload[0], e.payload[1]) : null))
    );
    const u6 = listen("import-done", () => {
      setImporting(null);
      scheduleReload(); // newly imported files should appear in the grid
    });
    const u7 = listen<[number, number]>("takeout-progress", (e) =>
      setTakeoutProg((prev) => (e.payload[1] > 0 ? merge(prev, e.payload[0], e.payload[1]) : null))
    );
    const u8 = listen("takeout-done", () => setTakeoutProg(null));
    // Any sync pass (manual, idle, background) that actually changed the local
    // library. The per-phase *-done events above are now only emitted when
    // their phase did work, so this is the reload signal for delete-only or
    // metadata-only changes that download nothing.
    const u9 = listen("library-changed", () => scheduleReload());
    return () => {
      if (reloadTimer) clearTimeout(reloadTimer);
      u0.then((f) => f()); u0b.then((f) => f());
      u1.then((f) => f()); u2.then((f) => f()); u3.then((f) => f()); u4.then((f) => f());
      u5.then((f) => f()); u6.then((f) => f()); u7.then((f) => f()); u8.then((f) => f());
      u9.then((f) => f());
    };
  }, []);

  const doSync = useCallback(async () => {
    setSyncing(true);
    try {
      const r = await api.sync();
      await refreshSession();
      // Refresh the lists only when the sync actually changed something — a
      // no-op sync must not make every view re-fetch and re-render.
      if (r.changes > 0) reload();
      showToast(`Synced — ${r.gallery} photos, ${r.albums} albums`);
    } catch (e) { showToast("Sync failed: " + e); }
    finally { setSyncing(false); }
  }, [refreshSession, showToast]);

  const cancelTakeout = useCallback(async () => {
    await api.cancelTakeout();
    showToast("Cancelling takeout…");
  }, [showToast]);
  const cancelImport = useCallback(async () => {
    await api.cancelImport();
    showToast("Cancelling import…");
  }, [showToast]);

  useEffect(() => {
    const un = listen("tray-sync", () => doSync());
    doSync(); // initial sync on load
    return () => { un.then((f) => f()); };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Drag files from the OS into the app → import them (encrypted on the way in).
  const [dropActive, setDropActive] = useState(false);
  useEffect(() => {
    let un: (() => void) | undefined;
    let cancelled = false;
    getCurrentWebview().onDragDropEvent((event) => {
      const p = event.payload as { type: string; paths?: string[] };
      // A native drag-OUT released back over our own window delivers a "drop" of
      // the decrypted temp export files. Ignore the whole gesture: those files are
      // already in the library, and importing them would race cleanup and hang.
      if (dragOutActive) { setDropActive(false); return; }
      if (p.type === "enter" || p.type === "over") setDropActive(true);
      else if (p.type === "leave") setDropActive(false);
      else if (p.type === "drop") {
        setDropActive(false);
        const paths = p.paths ?? [];
        if (paths.length) {
          api.importPaths(paths, null)
            .then((n) => {
              showToast(n > 0 ? `Imported ${n} item(s) — syncing…` : "Nothing new to import");
              if (n > 0) { reload(); doSync(); } // show now, sync in background
            })
            .catch((err) => showToast("Import failed: " + err));
        }
      }
    }).then((f) => { if (cancelled) f(); else un = f; });
    return () => { cancelled = true; un?.(); };
  }, [doSync, showToast]);

  // Ctrl/Cmd+V anywhere (outside text fields) → paste from the clipboard:
  // files (copied in Explorer or from the app) are imported; otherwise an image
  // (e.g. copied from Telegram) is imported.
  useEffect(() => {
    const onPaste = async (e: KeyboardEvent) => {
      if (!(e.ctrlKey || e.metaKey) || e.key.toLowerCase() !== "v") return;
      if (inTextField(e)) return;
      try {
        const files = await api.clipboardFiles().catch(() => []);
        if (files.length > 0) {
          const n = await api.importPaths(files, null);
          if (n > 0) { showToast(`Pasted ${n} item(s) — syncing…`); reload(); doSync(); }
          else showToast("Those items are already in your library");
          return;
        }
        const n = await api.pasteFromClipboard(null);
        if (n > 0) { showToast(`Pasted ${n} image — syncing…`); reload(); doSync(); }
        else showToast("Nothing to paste from the clipboard");
      } catch (err) { showToast("Paste failed: " + err); }
    };
    window.addEventListener("keydown", onPaste);
    return () => window.removeEventListener("keydown", onPaste);
  }, [doSync, showToast]);

  const pct = session.space_quota > 0 ? Math.max(0, Math.min(100, (session.space_used / session.space_quota) * 100)) : 0;

  return (
    <div className="app">
      <div className="sidebar">
        <div className="brand">
          <img className="brand-logo" src={logoUrl} alt="" />
          <div className="brand-title">Stingle Desktop</div>
        </div>
        {appVer && <div className="brand-version">v{appVer}</div>}
        <div className="nav">
          <button className={view === "gallery" ? "active" : ""} onClick={() => setView("gallery")}><GalleryIcon /> Gallery</button>
          <button className={view === "albums" ? "active" : ""} onClick={() => setView("albums")}><AlbumsIcon /> Albums</button>
          <button className={view === "sharing" ? "active" : ""} onClick={() => setView("sharing")}><SharingIcon /> Sharing</button>
          <button className={view === "contacts" ? "active" : ""} onClick={() => setView("contacts")}><ContactsIcon /> Contacts</button>
          <button className={view === "trash" ? "active" : ""} onClick={() => setView("trash")}><TrashIcon /> Trash</button>
          <button className={view === "settings" ? "active" : ""} onClick={() => setView("settings")}><SettingsIcon /> Settings</button>
        </div>
        <div className="spacer" />
        {updateVer && (
          <button className="update-card" disabled={updating} onClick={installUpdate}>
            <span className="update-card-title">
              ⤓ {updating ? "Installing update…" : "Update available"}
            </span>
            <span className="update-card-sub">
              {updating
                ? "The app will restart automatically"
                : `Version ${updateVer} is ready — click to restart and apply`}
            </span>
          </button>
        )}
        <SyncPanel
          syncing={syncing} upload={upload} thumbs={thumbs} originals={originals}
          importing={importing} takeout={takeoutProg}
          onCancelImport={cancelImport} onCancelTakeout={cancelTakeout}
        />
        <div className="side-div" />
        <div className="acct">{session.email}</div>
        <div className="storage-bar"><div style={{ width: pct + "%" }} /></div>
        <div className="acct">{fmtMB(session.space_used)} / {fmtMB(session.space_quota)}</div>
      </div>

      <div className="main">
        {view === "gallery" && <GalleryView reloadSignal={reloadKey} syncing={syncing} onSync={doSync} showToast={showToast} onChanged={reload} />}
        {view === "albums" && <AlbumsView reloadSignal={reloadKey} showToast={showToast} />}
        {view === "sharing" && <SharingView reloadSignal={reloadKey} showToast={showToast} />}
        {view === "contacts" && <ContactsView reloadSignal={reloadKey} showToast={showToast} />}
        {view === "trash" && <TrashView reloadSignal={reloadKey} showToast={showToast} onChanged={reload} />}
        {view === "settings" && <SettingsView session={session} setSession={setSession} showToast={showToast} />}
      </div>

      {toast && <div className="toast">{toast}</div>}
      {dropActive && (
        <div className="drop-overlay">
          <div className="drop-card">⤓ Drop photos &amp; videos to import</div>
        </div>
      )}
    </div>
  );
}

/* ----------------------------- Gallery ----------------------------- */

// Progressive load: render the first page almost immediately, then fetch the
// rest in the background and append it in chunks. The full array is kept once
// loaded, so selection / marquee / viewer index math is unaffected — only the
// first-paint latency and the cost of one giant IPC call are removed.
const FIRST_PAGE = 1000;
const REST_PAGE = 4000;
function usePagedFiles(
  fetchPage: (offset: number, limit: number) => Promise<FileItem[]>,
  reloadSignal: number,
): { items: FileItem[]; reload: () => void; remove: (filenames: string[]) => void } {
  const [items, setItems] = useState<FileItem[]>([]);
  // Latest committed list, read at reload time to tell a cold load (empty grid →
  // show progressively) from a warm reload (grid already populated → swap once at
  // the end, so a sync-triggered reload doesn't visibly shrink then regrow).
  const itemsRef = useRef(items);
  itemsRef.current = items;
  // Bumped on each (re)load so a superseded in-flight load stops updating state.
  const tokenRef = useRef(0);
  const reload = useCallback(() => {
    const token = ++tokenRef.current;
    const cold = itemsRef.current.length === 0;
    (async () => {
      const first = await fetchPage(0, FIRST_PAGE);
      if (token !== tokenRef.current) return;
      if (cold) setItems(first);
      // Dedupe by filename: a concurrent sync insert can shift OFFSET and make a
      // row reappear across a page boundary. Advance `offset` by the raw page
      // length so pagination stays aligned with the backend regardless.
      const seen = new Set(first.map((f) => f.filename));
      const acc = first.slice();
      let offset = first.length;
      let done = first.length < FIRST_PAGE; // first page was the whole list
      while (!done) {
        const page = await fetchPage(offset, REST_PAGE);
        if (token !== tokenRef.current) return;
        for (const f of page) {
          if (!seen.has(f.filename)) { seen.add(f.filename); acc.push(f); }
        }
        offset += page.length;
        done = page.length < REST_PAGE;
        if (cold) setItems(acc.slice()); // grow the grid as chunks arrive
      }
      if (!cold) setItems(acc); // warm: single atomic swap, no flicker
    })().catch(() => { /* leave the previous list in place on failure */ });
  }, [fetchPage]);
  useEffect(() => { reload(); }, [reload, reloadSignal]);
  // Optimistically drop rows (e.g. just-trashed files) so the grid updates
  // instantly, without waiting for a warm reload to page through the whole list
  // — which can be superseded mid-flight by a concurrent sync-triggered reload
  // and leave the deleted item on screen until a remount does a cold load.
  const remove = useCallback((filenames: string[]) => {
    const kill = new Set(filenames);
    setItems((prev) => prev.filter((f) => !kill.has(f.filename)));
  }, []);
  return { items, reload, remove };
}

function GalleryView({ syncing, onSync, showToast, onChanged, reloadSignal }: {
  syncing: boolean; onSync: () => void; showToast: (m: string) => void; onChanged: () => void; reloadSignal: number;
}) {
  const [sel, setSel] = useState<Set<string>>(new Set());
  const [viewerIdx, setViewerIdx] = useState<number | null>(null);

  const fetchPage = useCallback(
    (offset: number, limit: number) => api.listGallery(offset, limit), []);
  const { items, reload: load, remove } = usePagedFiles(fetchPage, reloadSignal);

  const doImport = async () => {
    const files = await pickFiles();
    if (files.length === 0) return;
    const n = await api.importPaths(files, null);
    showToast(`Imported ${n} file(s) — syncing…`);
    load();    // show the imported files immediately
    onSync();  // then sync in the background
  };

  const doImportFolder = async () => {
    const dir = await pickFolder();
    if (!dir) return;
    const n = await api.importPaths([dir], null);
    showToast(`Imported ${n} file(s) — syncing…`);
    load();
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
                onTrashed={remove}
                onDone={() => { setSel(new Set()); onChanged(); }} showToast={showToast} />
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
            sel={sel} setSel={setSel} onOpen={setViewerIdx} showToast={showToast} />
        )}
      </div>
      {viewerIdx !== null && (
        <Viewer items={items} index={viewerIdx} set={SET_GALLERY} albumId={null}
          onClose={() => setViewerIdx(null)} onChanged={onChanged}
          onTrashed={remove} showToast={showToast} />
      )}
    </>
  );
}

/* ----------------------------- Albums ----------------------------- */

function AlbumsView({ showToast, reloadSignal }: { showToast: (m: string) => void; reloadSignal: number }) {
  const [albums, setAlbums] = useState<Album[]>([]);
  const [open, setOpen] = useState<Album | null>(null);
  const contentRef = useRef<HTMLDivElement>(null);
  const scrollPos = useRef(0);
  const load = useCallback(() => { api.listAlbums().then(setAlbums); }, []);
  useEffect(() => { load(); }, [load, reloadSignal]);

  // Restore the list's scroll position after returning from an opened album.
  useLayoutEffect(() => {
    if (!open && contentRef.current) contentRef.current.scrollTop = scrollPos.current;
  }, [open]);

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
      <div className="content" ref={contentRef}>
        {albums.length === 0 ? <div className="empty">No albums yet.</div> : (
          <div className="albums">
            {albums.map((a) => (
              <div key={a.album_id} className="album-card"
                onClick={() => { scrollPos.current = contentRef.current?.scrollTop ?? 0; setOpen(a); }}>
                <div className="cover">
                  {a.cover === BLANK_COVER
                    ? <div className="blank-cover">🖼️</div>
                    : a.cover ? <img src={mediaUrl(SET_ALBUM, a.cover, true, a.album_id)} /> : "📁"}
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

function AlbumDetail({ album: initialAlbum, onBack, showToast }: { album: Album; onBack: () => void; showToast: (m: string) => void }) {
  // Local copy so share/unshare/perm changes reflect immediately without a full
  // albums-list round-trip. The parent list reloads on back.
  const [album, setAlbum] = useState<Album>(initialAlbum);
  const [items, setItems] = useState<FileItem[]>([]);
  const [viewerIdx, setViewerIdx] = useState<number | null>(null);
  const [sel, setSel] = useState<Set<string>>(new Set());
  const [showShare, setShowShare] = useState(false);
  const [showSharing, setShowSharing] = useState(false);

  // Permission-gated capabilities (mirrors the Android matrix). For an owned or
  // unshared album these are all true; for a received album they follow the
  // album's permission string.
  const P = parsePermissions(album.permissions);
  const canAdd = album.is_owner || (album.is_shared && P.add);
  const canCopy = album.is_owner || P.copy;
  const canShare = album.is_owner || (album.is_shared && P.share);
  const caps = { canCopy, canDelete: album.is_owner, canShare };

  const load = useCallback(() => { api.listAlbumFiles(album.album_id).then(setItems); }, [album.album_id]);
  useEffect(() => { load(); }, [load]);
  // Optimistically drop just-trashed rows so the grid updates immediately,
  // instead of waiting on the reload to round-trip.
  const remove = useCallback((filenames: string[]) => {
    const kill = new Set(filenames);
    setItems((prev) => prev.filter((f) => !kill.has(f.filename)));
  }, []);

  const clearSel = () => setSel(new Set());

  const addFiles = async () => {
    const files = await pickFiles();
    if (!files.length) return;
    const n = await api.importPaths(files, album.album_id);
    showToast(`Imported ${n} — syncing…`);
    load();                       // show immediately
    api.sync().then(() => load()); // sync in the background, refresh when done
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
  const blankCover = async () => {
    if (!confirm("Hide this album's contents behind a blank cover?")) return;
    await api.setAlbumBlankCover(album.album_id); showToast("Blank album cover set");
  };
  const leave = async () => {
    if (!confirm("Leave this shared album?")) return;
    await api.leaveAlbum(album.album_id); onBack();
  };

  // Stable so the memoized PhotoGrid doesn't re-render the whole album on viewer
  // open/close. Re-created only when ownership / album id / showToast change.
  const renderCover = useCallback((f: FileItem) => album.is_owner ? (
    <button className="cover-btn" onClick={(e) => {
      e.stopPropagation();
      api.setAlbumCover(album.album_id, f.filename).then(() => showToast("Album cover set"));
    }}>★ cover</button>
  ) : null, [album.is_owner, album.album_id, showToast]);

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
                caps={caps} onTrashed={remove}
                onDone={() => { clearSel(); load(); }} showToast={showToast} />
              <button onClick={clearSel}>Cancel</button>
            </>
          ) : (
            <>
              {canAdd && <button onClick={addFiles}>＋ Add</button>}
              {album.is_owner && !album.is_shared && <button onClick={() => setShowShare(true)}>👥 Share</button>}
              {album.is_shared && <button onClick={() => setShowSharing(true)}>{album.is_owner ? "👥 Sharing" : "ℹ Info"}</button>}
              {album.is_owner && <button onClick={rename}>Rename</button>}
              {album.is_owner && <button onClick={blankCover}>Blank cover</button>}
              {album.is_owner && <button onClick={del}>Delete</button>}
              {!album.is_owner && <button onClick={leave}>Leave</button>}
            </>
          )}
        </div>
      </div>
      <div className="content">
        {items.length === 0 ? <div className="empty">Empty album.</div> : (
          <PhotoGrid items={items} set={SET_ALBUM} albumId={album.album_id} grouped={false}
            sel={sel} setSel={setSel} onOpen={setViewerIdx} showToast={showToast}
            renderExtra={renderCover} />
        )}
      </div>
      {viewerIdx !== null && (
        <Viewer items={items} index={viewerIdx} set={SET_ALBUM} albumId={album.album_id} caps={caps}
          onClose={() => setViewerIdx(null)} onChanged={load} onTrashed={remove} showToast={showToast} />
      )}
      {showShare && (
        <ShareDialog album={album}
          onClose={() => setShowShare(false)}
          onShared={(perms) => { setAlbum((a) => ({ ...a, is_shared: true, permissions: perms })); }}
          showToast={showToast} />
      )}
      {showSharing && (
        <AlbumSharingDialog album={album} canShare={canShare}
          onClose={() => setShowSharing(false)}
          onChanged={(perms) => { if (perms) setAlbum((a) => ({ ...a, permissions: perms })); }}
          onUnshared={() => { setAlbum((a) => ({ ...a, is_shared: false, permissions: "" })); setShowSharing(false); }}
          showToast={showToast} />
      )}
    </>
  );
}

/* ----------------------------- Sharing ----------------------------- */

/** Share dialog. Fresh-share mode (owner, not-yet-shared) is a two-step wizard —
 *  pick recipients, then set permissions. Add-mode (adding people to an already
 *  shared album) is one step and reuses the album's current permissions, matching
 *  Android's onlyAddMembers flow. */
function ShareDialog({ album, onClose, onShared, showToast, addMode = false, excludeEmails = [] }: {
  album: Album;
  onClose: () => void;
  onShared: (permissions: string) => void;
  showToast: (m: string) => void;
  addMode?: boolean;
  excludeEmails?: string[];
}) {
  const [step, setStep] = useState<1 | 2>(1);
  const [contacts, setContacts] = useState<Contact[]>([]);
  const [query, setQuery] = useState("");
  const [recipients, setRecipients] = useState<string[]>([]); // emails
  const [allowAdd, setAllowAdd] = useState(true);
  const [allowShare, setAllowShare] = useState(true);
  const [allowCopy, setAllowCopy] = useState(true);
  const [busy, setBusy] = useState(false);

  const excluded = useMemo(
    () => new Set(excludeEmails.map((x) => x.toLowerCase())),
    [excludeEmails]
  );

  useEffect(() => {
    api.listContacts().then(setContacts).catch(() => setContacts([]));
    const h = (e: KeyboardEvent) => { if (e.key === "Escape") onClose(); };
    window.addEventListener("keydown", h);
    return () => window.removeEventListener("keydown", h);
  }, [onClose]);

  const add = (email: string) => {
    const e = email.trim().toLowerCase();
    if (!e) return;
    if (!/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(e)) { showToast("Enter a valid email"); return; }
    if (excluded.has(e)) { showToast("Already a member"); return; }
    if (recipients.includes(e)) return;
    setRecipients((r) => [...r, e]);
    setQuery("");
  };
  const removeRcpt = (email: string) => setRecipients((r) => r.filter((x) => x !== email));

  const q = query.trim().toLowerCase();
  const suggestions = contacts
    .filter((c) => !recipients.includes(c.email.toLowerCase()))
    .filter((c) => !excluded.has(c.email.toLowerCase()))
    .filter((c) => !q || c.email.toLowerCase().includes(q));

  const doShare = async () => {
    if (!recipients.length) return;
    // Add-mode keeps the album's existing album-wide permissions untouched.
    const cur = parsePermissions(album.permissions);
    const [a, s, cp] = addMode
      ? [cur.add, cur.share, cur.copy]
      : [allowAdd, allowShare, allowCopy];
    setBusy(true);
    try {
      await api.shareAlbum(album.album_id, recipients, a, s, cp);
      onShared(`1${a ? 1 : 0}${s ? 1 : 0}${cp ? 1 : 0}`);
      showToast(`${addMode ? "Added" : "Shared with"} ${recipients.length} ${recipients.length === 1 ? "person" : "people"}`);
      onClose();
    } catch (err) {
      showToast((addMode ? "Add" : "Share") + " failed: " + err);
      setBusy(false);
    }
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal share-dialog" onClick={(e) => e.stopPropagation()}>
        <div className="move-head">
          <h3>{addMode ? "Add people to" : "Share"} “{album.name}”</h3>
          <button className="icon-btn" onClick={onClose}>✕</button>
        </div>

        {step === 1 ? (
          <>
            <div className="chips">
              {recipients.map((r) => (
                <span key={r} className="chip">{r}<button onClick={() => removeRcpt(r)}>✕</button></span>
              ))}
              {recipients.length === 0 && <span className="muted">No recipients yet.</span>}
            </div>
            <div className="share-add">
              <input autoFocus placeholder="Email address" value={query}
                onChange={(e) => setQuery(e.target.value)}
                onKeyDown={(e) => { if (e.key === "Enter") add(query); }} />
              <button onClick={() => add(query)}>Add</button>
            </div>
            {suggestions.length > 0 && (
              <div className="contact-list">
                {suggestions.map((c) => (
                  <button key={c.user_id} className="contact-row" onClick={() => add(c.email)}>
                    <span className="avatar">{c.email[0]?.toUpperCase()}</span>
                    <span className="c-email">{c.email}</span>
                    <span className="c-add">＋</span>
                  </button>
                ))}
              </div>
            )}
            <div className="modal-actions">
              <button onClick={onClose} disabled={busy}>Cancel</button>
              {addMode ? (
                <button className="primary" disabled={!recipients.length || busy} onClick={doShare}>{busy ? "Adding…" : "Add"}</button>
              ) : (
                <button className="primary" disabled={!recipients.length} onClick={() => setStep(2)}>Next</button>
              )}
            </div>
          </>
        ) : (
          <>
            <p className="muted">Sharing with {recipients.length} {recipients.length === 1 ? "person" : "people"}. Choose what they can do:</p>
            <PermToggles
              allowAdd={allowAdd} allowShare={allowShare} allowCopy={allowCopy}
              setAllowAdd={setAllowAdd} setAllowShare={setAllowShare} setAllowCopy={setAllowCopy} />
            <div className="modal-actions">
              <button onClick={() => setStep(1)} disabled={busy}>Back</button>
              <button className="primary" onClick={doShare} disabled={busy}>{busy ? "Sharing…" : "Share"}</button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}

/** Share loose files (a gallery/album selection, or one item in the viewer) by
 *  auto-creating an album and sharing it. Two steps: recipients, then album name
 *  + permissions. Mirrors Android's loose-files share. */
function ShareFilesDialog({ set, albumId, filenames, onClose, onShared, showToast }: {
  set: number;
  albumId: string | null;
  filenames: string[];
  onClose: () => void;
  onShared: () => void;
  showToast: (m: string) => void;
}) {
  const defaultName = useMemo(
    () => new Date().toLocaleDateString(undefined, { year: "numeric", month: "long", day: "numeric" }),
    []
  );
  const [step, setStep] = useState<1 | 2>(1);
  const [contacts, setContacts] = useState<Contact[]>([]);
  const [query, setQuery] = useState("");
  const [recipients, setRecipients] = useState<string[]>([]);
  const [name, setName] = useState(defaultName);
  const [allowAdd, setAllowAdd] = useState(true);
  const [allowShare, setAllowShare] = useState(true);
  const [allowCopy, setAllowCopy] = useState(true);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api.listContacts().then(setContacts).catch(() => setContacts([]));
    const h = (e: KeyboardEvent) => { if (e.key === "Escape") onClose(); };
    window.addEventListener("keydown", h);
    return () => window.removeEventListener("keydown", h);
  }, [onClose]);

  const add = (email: string) => {
    const e = email.trim().toLowerCase();
    if (!e) return;
    if (!/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(e)) { showToast("Enter a valid email"); return; }
    if (recipients.includes(e)) return;
    setRecipients((r) => [...r, e]);
    setQuery("");
  };
  const removeRcpt = (email: string) => setRecipients((r) => r.filter((x) => x !== email));

  const q = query.trim().toLowerCase();
  const suggestions = contacts
    .filter((c) => !recipients.includes(c.email.toLowerCase()))
    .filter((c) => !q || c.email.toLowerCase().includes(q));

  const doShare = async () => {
    if (!recipients.length) return;
    setBusy(true);
    try {
      await api.shareNewAlbum(set, albumId, filenames, name.trim() || defaultName,
        recipients, allowAdd, allowShare, allowCopy);
      showToast(`Shared ${filenames.length} item${filenames.length === 1 ? "" : "s"} with ${recipients.length} ${recipients.length === 1 ? "person" : "people"}`);
      onShared();
    } catch (err) {
      showToast("Share failed: " + err);
      setBusy(false);
    }
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal share-dialog" onClick={(e) => e.stopPropagation()}>
        <div className="move-head">
          <h3>Share {filenames.length} item{filenames.length === 1 ? "" : "s"}</h3>
          <button className="icon-btn" onClick={onClose}>✕</button>
        </div>

        {step === 1 ? (
          <>
            <div className="chips">
              {recipients.map((r) => (
                <span key={r} className="chip">{r}<button onClick={() => removeRcpt(r)}>✕</button></span>
              ))}
              {recipients.length === 0 && <span className="muted">Who do you want to share with?</span>}
            </div>
            <div className="share-add">
              <input autoFocus placeholder="Email address" value={query}
                onChange={(e) => setQuery(e.target.value)}
                onKeyDown={(e) => { if (e.key === "Enter") add(query); }} />
              <button onClick={() => add(query)}>Add</button>
            </div>
            {suggestions.length > 0 && (
              <div className="contact-list">
                {suggestions.map((c) => (
                  <button key={c.user_id} className="contact-row" onClick={() => add(c.email)}>
                    <span className="avatar" style={{ background: avatarColor(c.email) }}>{c.email[0]?.toUpperCase()}</span>
                    <span className="c-email">{c.email}</span>
                    <span className="c-add">＋</span>
                  </button>
                ))}
              </div>
            )}
            <div className="modal-actions">
              <button onClick={onClose}>Cancel</button>
              <button className="primary" disabled={!recipients.length} onClick={() => setStep(2)}>Next</button>
            </div>
          </>
        ) : (
          <>
            <label className="share-name-label">Album name</label>
            <input className="share-name-input" value={name} onChange={(e) => setName(e.target.value)} />
            <PermToggles
              allowAdd={allowAdd} allowShare={allowShare} allowCopy={allowCopy}
              setAllowAdd={setAllowAdd} setAllowShare={setAllowShare} setAllowCopy={setAllowCopy} />
            <div className="modal-actions">
              <button onClick={() => setStep(1)} disabled={busy}>Back</button>
              <button className="primary" onClick={doShare} disabled={busy}>{busy ? "Sharing…" : "Share"}</button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}

/** The three permission rows. Editable (checkboxes) for owners, or a read-only
 *  Yes/No summary for members viewing album info. */
function PermToggles({ allowAdd, allowShare, allowCopy, setAllowAdd, setAllowShare, setAllowCopy, readOnly = false }: {
  allowAdd: boolean; allowShare: boolean; allowCopy: boolean;
  setAllowAdd?: (v: boolean) => void; setAllowShare?: (v: boolean) => void; setAllowCopy?: (v: boolean) => void;
  readOnly?: boolean;
}) {
  const Row = (label: string, desc: string, val: boolean, set?: (v: boolean) => void) => (
    <label className="perm-row">
      <div>
        <div className="perm-label">{label}</div>
        <div className="perm-desc muted">{desc}</div>
      </div>
      {readOnly
        ? <span className={"perm-badge " + (val ? "yes" : "no")}>{val ? "Yes" : "No"}</span>
        : <input type="checkbox" checked={val} onChange={(e) => set?.(e.target.checked)} />}
    </label>
  );
  return (
    <div className="perm-list">
      {Row("Allow adding photos", "Members can add photos and videos to this album.", allowAdd, setAllowAdd)}
      {Row("Allow re-sharing", "Members can share this album with other people.", allowShare, setAllowShare)}
      {Row("Allow copying", "Members can save and export copies of the photos.", allowCopy, setAllowCopy)}
    </div>
  );
}

/** Unified album sharing panel. For the owner it's editable (members add/remove,
 *  permission toggles, unshare); for a member it's read-only info, plus an
 *  "Add people" action when they hold the re-share permission. Mirrors Android's
 *  AlbumSettingsDialogFragment / AlbumInfoDialogFragment. */
function AlbumSharingDialog({ album, canShare, onClose, onChanged, onUnshared, showToast }: {
  album: Album;
  canShare: boolean;
  onClose: () => void;
  onChanged: (permissions?: string) => void;
  onUnshared: () => void;
  showToast: (m: string) => void;
}) {
  const isOwner = album.is_owner;
  const [members, setMembers] = useState<AlbumMember[]>([]);
  const perms = parsePermissions(album.permissions);
  const [allowAdd, setAllowAdd] = useState(perms.add);
  const [allowShare, setAllowShare] = useState(perms.share);
  const [allowCopy, setAllowCopy] = useState(perms.copy);
  const [busy, setBusy] = useState(false);
  const [showAdd, setShowAdd] = useState(false);
  const [confirmUnshare, setConfirmUnshare] = useState(false);

  const load = useCallback(() => {
    api.listAlbumMembers(album.album_id).then(setMembers).catch(() => setMembers([]));
  }, [album.album_id]);
  useEffect(() => { load(); }, [load]);
  useEffect(() => {
    // Only close on Escape when no nested dialog is on top — otherwise a single
    // Escape would dismiss both (all use window listeners).
    const h = (e: KeyboardEvent) => { if (e.key === "Escape" && !showAdd && !confirmUnshare) onClose(); };
    window.addEventListener("keydown", h);
    return () => window.removeEventListener("keydown", h);
  }, [onClose, showAdd, confirmUnshare]);

  const dirty = allowAdd !== perms.add || allowShare !== perms.share || allowCopy !== perms.copy;

  const savePerms = async () => {
    setBusy(true);
    try {
      await api.editAlbumPerms(album.album_id, allowAdd, allowShare, allowCopy);
      onChanged(`1${allowAdd ? 1 : 0}${allowShare ? 1 : 0}${allowCopy ? 1 : 0}`);
      showToast("Permissions updated");
    } catch (err) { showToast("Update failed: " + err); }
    setBusy(false);
  };

  const remove = async (m: AlbumMember) => {
    if (!confirm(`Remove ${m.email ?? "this member"} from the album?`)) return;
    try {
      await api.removeAlbumMember(album.album_id, m.user_id);
      showToast("Member removed");
      load();
    } catch (err) { showToast("Remove failed: " + err); }
  };

  const unshare = async () => {
    setConfirmUnshare(false);
    try {
      await api.unshareAlbum(album.album_id);
      showToast("Unshared");
      onUnshared();
    } catch (err) { showToast("Unshare failed: " + err); }
  };

  const memberEmails = members.map((m) => m.email).filter((x): x is string => !!x);

  return (
    <>
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal share-dialog" onClick={(e) => e.stopPropagation()}>
        <div className="move-head">
          <h3>{isOwner ? "Sharing" : "Album info"} — “{album.name}”</h3>
          <button className="icon-btn" onClick={onClose}>✕</button>
        </div>

        <h4 className="perm-head">Members</h4>
        <div className="contact-list">
          {members.length === 0 && <span className="muted">No members yet.</span>}
          {members.map((m) => {
            const label = m.is_owner ? "Me" : (m.email ?? `User ${m.user_id}`);
            return (
              <div key={m.user_id} className="contact-row static">
                <span className="avatar">{label[0]?.toUpperCase()}</span>
                <span className="c-email">{label}</span>
                {isOwner && !m.is_owner && <button className="c-remove" onClick={() => remove(m)}>Remove</button>}
              </div>
            );
          })}
        </div>
        {canShare && (
          <button onClick={() => setShowAdd(true)}>＋ Add people</button>
        )}

        <h4 className="perm-head">Permissions</h4>
        <PermToggles
          allowAdd={allowAdd} allowShare={allowShare} allowCopy={allowCopy}
          setAllowAdd={setAllowAdd} setAllowShare={setAllowShare} setAllowCopy={setAllowCopy}
          readOnly={!isOwner} />

        <div className="modal-actions">
          {isOwner && <button className="danger" onClick={() => setConfirmUnshare(true)} style={{ marginRight: "auto" }}>Unshare</button>}
          <button onClick={onClose}>Close</button>
          {isOwner && <button className="primary" onClick={savePerms} disabled={busy || !dirty}>{busy ? "Saving…" : "Save permissions"}</button>}
        </div>
      </div>
    </div>

      {showAdd && (
        <ShareDialog album={album} addMode excludeEmails={memberEmails}
          onClose={() => setShowAdd(false)}
          onShared={() => { load(); }}
          showToast={showToast} />
      )}
      {confirmUnshare && (
        <ConfirmDialog
          title="Stop sharing this album?"
          message="Everyone you shared it with will lose access."
          onClose={() => setConfirmUnshare(false)}
          actions={[
            { label: "Cancel", onClick: () => setConfirmUnshare(false) },
            { label: "Unshare", variant: "danger", onClick: unshare },
          ]}
        />
      )}
    </>
  );
}

const AVATAR_COLORS = ["#378add", "#1d9e75", "#d85a30", "#7f77dd", "#d4537e", "#ba7517"];
/** Deterministic avatar color from a seed (email or user-id). */
function avatarColor(seed: string): string {
  let h = 0;
  for (let i = 0; i < seed.length; i++) h = (h * 31 + seed.charCodeAt(i)) | 0;
  return AVATAR_COLORS[Math.abs(h) % AVATAR_COLORS.length];
}

function SharingView({ showToast, reloadSignal }: { showToast: (m: string) => void; reloadSignal: number }) {
  const [albums, setAlbums] = useState<SharedAlbum[]>([]);
  const [open, setOpen] = useState<SharedAlbum | null>(null);
  const load = useCallback(() => { api.listSharedAlbums().then(setAlbums); }, []);
  useEffect(() => { load(); }, [load, reloadSignal]);

  if (open) return <AlbumDetail album={open} onBack={() => { setOpen(null); load(); }} showToast={showToast} />;

  return (
    <>
      <div className="topbar"><h2>Sharing</h2></div>
      <div className="content">
        {albums.length === 0 ? (
          <div className="empty">Nothing shared yet. Share an album, or albums shared with you will appear here after a sync.</div>
        ) : (
          <div className="share-grid">
            {albums.map((a) => {
              const others = a.members.filter((m) => !m.is_owner);
              return (
                <div key={a.album_id} className="share-card" onClick={() => setOpen(a)}>
                  <div className="cover">
                    {a.cover === BLANK_COVER
                      ? <div className="blank-cover">🖼️</div>
                      : a.cover ? <img src={mediaUrl(SET_ALBUM, a.cover, true, a.album_id)} /> : "📁"}
                    <span className={"sc-badge " + (a.is_owner ? "sent" : "recv")}>
                      {a.is_owner ? "↗ Shared by you" : "↙ Shared with you"}
                    </span>
                  </div>
                  <div className="sc-body">
                    <div className="sc-name">{a.name}</div>
                    <div className="sc-foot">
                      {others.length > 0 ? (
                        <>
                          <div className="sc-stack">
                            {others.slice(0, 3).map((m) => (
                              <span key={m.user_id} className="av"
                                style={{ background: avatarColor(m.email ?? m.user_id) }}
                                title={m.email ?? `User ${m.user_id}`}>
                                {(m.email?.[0] ?? "?").toUpperCase()}
                              </span>
                            ))}
                            {others.length > 3 && <span className="av more">+{others.length - 3}</span>}
                          </div>
                          <span className="sc-count">{others.length} {others.length === 1 ? "person" : "people"}</span>
                        </>
                      ) : (
                        <span className="sc-count">No other members</span>
                      )}
                    </div>
                  </div>
                </div>
              );
            })}
          </div>
        )}
      </div>
    </>
  );
}

/* ----------------------------- Contacts ----------------------------- */

function ContactsView({ showToast, reloadSignal }: { showToast: (m: string) => void; reloadSignal: number }) {
  const [contacts, setContacts] = useState<Contact[]>([]);
  const [shared, setShared] = useState<SharedAlbum[]>([]);
  const [query, setQuery] = useState("");
  const [selected, setSelected] = useState<Contact | null>(null);
  const [openAlbum, setOpenAlbum] = useState<SharedAlbum | null>(null);

  const load = useCallback(() => {
    api.listContacts().then(setContacts).catch(() => setContacts([]));
    api.listSharedAlbums().then(setShared).catch(() => setShared([]));
  }, []);
  useEffect(() => { load(); }, [load, reloadSignal]);

  // A contact's relationship: albums you own & shared with them (actionable), and
  // shared albums you both belong to (you don't own — open only).
  const relFor = useCallback((c: Contact) => ({
    byMe: shared.filter((a) => a.is_owner && a.members.some((m) => m.user_id === c.user_id)),
    bothIn: shared.filter((a) => !a.is_owner && a.members.some((m) => m.user_id === c.user_id)),
  }), [shared]);

  if (openAlbum) {
    return <AlbumDetail album={openAlbum} showToast={showToast}
      onBack={() => { setOpenAlbum(null); load(); }} />;
  }
  if (selected) {
    // Re-resolve against the freshest data so actions reflect immediately.
    const fresh = contacts.find((c) => c.user_id === selected.user_id) ?? selected;
    return <ContactDetail contact={fresh} rel={relFor(fresh)}
      onBack={() => { setSelected(null); load(); }}
      onOpenAlbum={setOpenAlbum} onChanged={load} showToast={showToast} />;
  }

  const q = query.trim().toLowerCase();
  const shown = contacts.filter((c) => !q || c.email.toLowerCase().includes(q));

  return (
    <>
      <div className="topbar">
        <h2>Contacts</h2>
        <div className="actionbar">
          <input className="contact-search" placeholder="Search contacts…" value={query}
            onChange={(e) => setQuery(e.target.value)} />
        </div>
      </div>
      <div className="content">
        {contacts.length === 0 ? (
          <div className="empty">No contacts yet. People you share albums with — and who share with you — appear here after a sync.</div>
        ) : shown.length === 0 ? (
          <div className="empty">No contacts match “{query}”.</div>
        ) : (
          <div className="contacts-grid">
            {shown.map((c) => {
              const { byMe, bothIn } = relFor(c);
              const parts = [
                byMe.length ? `You share ${byMe.length}` : null,
                bothIn.length ? `${bothIn.length} in common` : null,
              ].filter(Boolean);
              return (
                <div key={c.user_id} className="contact-card" onClick={() => setSelected(c)}>
                  <span className="avatar big" style={{ background: avatarColor(c.email) }}>{c.email[0]?.toUpperCase()}</span>
                  <div className="c-email" title={c.email}>{c.email}</div>
                  <div className="c-sub muted">{parts.length ? parts.join(" · ") : "No shared albums"}</div>
                </div>
              );
            })}
          </div>
        )}
      </div>
    </>
  );
}

function ContactDetail({ contact, rel, onBack, onOpenAlbum, onChanged, showToast }: {
  contact: Contact;
  rel: { byMe: SharedAlbum[]; bothIn: SharedAlbum[] };
  onBack: () => void;
  onOpenAlbum: (a: SharedAlbum) => void;
  onChanged: () => void;
  showToast: (m: string) => void;
}) {
  const [showShare, setShowShare] = useState(false);
  const [confirmRevoke, setConfirmRevoke] = useState(false);

  const removeFrom = async (a: SharedAlbum) => {
    if (!confirm(`Remove ${contact.email} from “${a.name}”?`)) return;
    try {
      await api.removeAlbumMember(a.album_id, contact.user_id);
      showToast(`Removed from ${a.name}`);
      onChanged();
    } catch (e) { showToast("Remove failed: " + e); }
  };

  const revokeAll = async () => {
    setConfirmRevoke(false);
    if (!rel.byMe.length) return;
    let ok = 0;
    for (const a of rel.byMe) {
      try { await api.removeAlbumMember(a.album_id, contact.user_id); ok++; } catch { /* keep going */ }
    }
    showToast(`Stopped sharing ${ok} album${ok === 1 ? "" : "s"} with ${contact.email}`);
    onChanged();
  };

  const row = (a: SharedAlbum, owned: boolean) => (
    <div key={a.album_id} className="rel-row">
      <div className="rel-cover" onClick={() => onOpenAlbum(a)}>
        {a.cover === BLANK_COVER ? <span>🖼️</span>
          : a.cover ? <img src={mediaUrl(SET_ALBUM, a.cover, true, a.album_id)} /> : <span>📁</span>}
      </div>
      <div className="rel-main" onClick={() => onOpenAlbum(a)}>
        <div className="rel-name">{a.name}</div>
        <div className="rel-sub muted">{a.count} item{a.count === 1 ? "" : "s"}</div>
      </div>
      {owned
        ? <button className="c-remove" onClick={() => removeFrom(a)}>Remove</button>
        : <button onClick={() => onOpenAlbum(a)}>Open</button>}
    </div>
  );

  return (
    <>
      <div className="topbar">
        <button onClick={onBack}>←</button>
        <h2>{contact.email}</h2>
      </div>
      <div className="content">
        <div className="rel-wrap">
          <div className="rel-headcard">
            <span className="avatar big" style={{ background: avatarColor(contact.email) }}>{contact.email[0]?.toUpperCase()}</span>
            <div className="rel-headinfo">
              <div className="rel-heademail">{contact.email}</div>
              {contact.date_used > 0 && <div className="muted rel-headdate">Last shared {dateLabel(contact.date_used)}</div>}
            </div>
            <div className="rel-headactions">
              <button className="primary" onClick={() => setShowShare(true)}>＋ Share an album</button>
              {rel.byMe.length > 0 && <button className="danger" onClick={() => setConfirmRevoke(true)}>Stop sharing all</button>}
            </div>
          </div>

          <h3 className="rel-sec">Albums you share with them</h3>
          {rel.byMe.length === 0
            ? <div className="rel-empty muted">You haven't shared any albums with {contact.email}.</div>
            : <div className="rel-list">{rel.byMe.map((a) => row(a, true))}</div>}

          <h3 className="rel-sec">Shared albums you're both in</h3>
          {rel.bothIn.length === 0
            ? <div className="rel-empty muted">No albums shared with you that {contact.email} is also in.</div>
            : <div className="rel-list">{rel.bothIn.map((a) => row(a, false))}</div>}
        </div>
      </div>

      {showShare && (
        <ShareAlbumToContactDialog contact={contact}
          alreadyShared={new Set(rel.byMe.map((a) => a.album_id))}
          onClose={() => setShowShare(false)}
          onShared={() => { setShowShare(false); onChanged(); }}
          showToast={showToast} />
      )}
      {confirmRevoke && (
        <ConfirmDialog
          title={`Stop sharing with ${contact.email}?`}
          message={`They'll lose access to all ${rel.byMe.length} album${rel.byMe.length === 1 ? "" : "s"} you've shared with them.`}
          onClose={() => setConfirmRevoke(false)}
          actions={[
            { label: "Cancel", onClick: () => setConfirmRevoke(false) },
            { label: "Stop sharing", variant: "danger", onClick: revokeAll },
          ]}
        />
      )}
    </>
  );
}

/** Pick one of your albums to share with a specific contact. */
function ShareAlbumToContactDialog({ contact, alreadyShared, onClose, onShared, showToast }: {
  contact: Contact;
  alreadyShared: Set<string>;
  onClose: () => void;
  onShared: () => void;
  showToast: (m: string) => void;
}) {
  const [albums, setAlbums] = useState<Album[]>([]);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    api.listAlbums().then(setAlbums).catch(() => setAlbums([]));
    const h = (e: KeyboardEvent) => { if (e.key === "Escape") onClose(); };
    window.addEventListener("keydown", h);
    return () => window.removeEventListener("keydown", h);
  }, [onClose]);

  // Only your own albums you haven't already shared with this person.
  const candidates = albums.filter((a) => a.is_owner && !alreadyShared.has(a.album_id));

  const share = async (a: Album) => {
    if (busy) return;
    setBusy(true);
    // Preserve an already-shared album's permissions; default all-on for a fresh share.
    const p = a.is_shared ? parsePermissions(a.permissions) : { add: true, share: true, copy: true };
    try {
      await api.shareAlbum(a.album_id, [contact.email], p.add, p.share, p.copy);
      showToast(`Shared “${a.name}” with ${contact.email}`);
      onShared();
    } catch (e) { showToast("Share failed: " + e); setBusy(false); }
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal move-dialog" onClick={(e) => e.stopPropagation()}>
        <div className="move-head">
          <h3>Share an album with {contact.email}</h3>
          <button className="icon-btn" onClick={onClose}>✕</button>
        </div>
        {candidates.length === 0 ? (
          <p className="muted">No albums left to share with this person.</p>
        ) : (
          <div className="move-grid">
            {candidates.map((a) => (
              <div key={a.album_id} className="move-card" onClick={() => share(a)}>
                <div className="move-cover">
                  {a.cover === BLANK_COVER ? <span>🖼️</span>
                    : a.cover ? <img src={mediaUrl(SET_ALBUM, a.cover, true, a.album_id)} /> : <span>📁</span>}
                </div>
                <div className="move-name" title={a.name}>{a.name}</div>
                <div className="move-count">{a.count} item{a.count === 1 ? "" : "s"}{a.is_shared ? " · shared" : ""}</div>
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
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
            <button onClick={async () => { await api.restore([...sel]); setSel(new Set()); showToast("Restored"); onChanged(); }}>Restore</button>
            <button onClick={async () => { if (confirm("Delete forever?")) { await api.deletePermanently([...sel]); setSel(new Set()); load(); } }}>Delete</button>
            <button onClick={() => setSel(new Set())}>Cancel</button>
          </>}
          {sel.size === 0 && items.length > 0 && <button onClick={async () => { if (confirm("Empty trash?")) { await api.emptyTrash(); load(); } }}>Empty Trash</button>}
        </div>
      </div>
      <div className="content">
        {items.length === 0 ? <div className="empty">Trash is empty.</div> : (
          <PhotoGrid items={items} set={SET_TRASH} albumId={null} grouped
            sel={sel} setSel={setSel} showToast={showToast}
            onOpen={(idx) => setSel(new Set([items[idx].filename]))} />
        )}
      </div>
    </>
  );
}

/* ----------------------------- Settings ----------------------------- */

type SettingsTab = "account" | "general" | "sync" | "storage" | "about";

const SETTINGS_TABS: { id: SettingsTab; label: string }[] = [
  { id: "account", label: "Account" },
  { id: "general", label: "General" },
  { id: "sync", label: "Sync" },
  { id: "storage", label: "Storage" },
  { id: "about", label: "About" },
];

function SettingsView({ session, setSession, showToast }: {
  session: Session; setSession: (s: Session | null) => void; showToast: (m: string) => void;
}) {
  const [phrase, setPhrase] = useState<string | null>(null);
  const [cacheLimit, setCacheLimit] = useState<string>("0");
  const [cacheSizeMB, setCacheSizeMB] = useState<number | null>(null);

  // App options
  const [autostart, setAutostart] = useState(false);
  const [startMin, setStartMin] = useState(false);
  const [minTray, setMinTray] = useState(false);
  const [autoUpdate, setAutoUpdate] = useState(true);
  const [convertHeic, setConvertHeic] = useState(true);
  const [syncEvery, setSyncEvery] = useState(false);
  const [autoSync, setAutoSync] = useState(true);
  const [autoSyncMins, setAutoSyncMins] = useState(10);
  const [autoUnlock, setAutoUnlock] = useState(false);
  const [secureStore, setSecureStore] = useState({ biometric: false, keyring: false });
  const [auPrompt, setAuPrompt] = useState(false);
  const [auPassword, setAuPassword] = useState("");
  const [storagePath, setStoragePath] = useState("");
  const [moving, setMoving] = useState<{ done: number; total: number } | null>(null);
  const [confirmSignOut, setConfirmSignOut] = useState(false);
  const [tab, setTab] = useState<SettingsTab>("account");

  // Watch folders
  const [watchFolders, setWatchFolders] = useState<WatchFolder[]>([]);
  const [watchStatus, setWatchStatus] = useState<string | null>(null);

  // About / updates
  const [version, setVersion] = useState("");
  const [checkingUpdate, setCheckingUpdate] = useState(false);

  const refreshCache = () => { api.cacheSize().then((b) => setCacheSizeMB(b / 1048576)); };
  useEffect(() => {
    api.getCacheLimit().then((b) => setCacheLimit(String(Math.round(b / 1048576))));
    refreshCache();
    api.getAutostart().then(setAutostart).catch(() => {});
    api.getStartMinimized().then(setStartMin).catch(() => {});
    api.getMinimizeToTray().then(setMinTray).catch(() => {});
    api.getAutoUpdate().then(setAutoUpdate).catch(() => {});
    api.getConvertHeicOnExport().then(setConvertHeic).catch(() => {});
    api.getAppVersion().then(setVersion).catch(() => {});
    api.getSyncEverything().then(setSyncEvery).catch(() => {});
    api.getAutoSync().then(setAutoSync).catch(() => {});
    api.getAutoSyncInterval().then(setAutoSyncMins).catch(() => {});
    api.isAutoUnlockEnabled().then(setAutoUnlock).catch(() => {});
    api.secureStoreStatus().then(setSecureStore).catch(() => {});
    api.getStoragePath().then(setStoragePath).catch(() => {});
    api.getWatchFolders().then(setWatchFolders).catch(() => {});
  }, []);

  // Progress bar while the storage folder is being moved.
  useEffect(() => {
    const u = listen<[number, number]>("storage-move-progress", (e) =>
      setMoving({ done: e.payload[0], total: e.payload[1] })
    );
    return () => { u.then((f) => f()); };
  }, []);

  // Live status from the watch-folder importer.
  useEffect(() => {
    const u1 = listen<string>("watch-import-progress", (e) =>
      setWatchStatus(`Imported ${e.payload}`)
    );
    const u2 = listen<string>("watch-import-error", (e) => setWatchStatus(e.payload));
    return () => { u1.then((f) => f()); u2.then((f) => f()); };
  }, []);

  // Persist the watch list and reflect it locally.
  const saveWatchFolders = async (next: WatchFolder[]) => {
    setWatchFolders(next);
    try { await api.setWatchFolders(next); }
    catch (e) { showToast("Failed to save watch folders: " + e); }
  };
  const addWatchFolder = async () => {
    const dir = await pickFolder();
    if (!dir) return;
    if (watchFolders.some((w) => w.path === dir)) { showToast("Already watching that folder"); return; }
    await saveWatchFolders([...watchFolders, { path: dir, delete_originals: false }]);
  };
  const removeWatchFolder = (path: string) =>
    saveWatchFolders(watchFolders.filter((w) => w.path !== path));
  const toggleDeleteOriginals = (path: string, v: boolean) =>
    saveWatchFolders(watchFolders.map((w) => (w.path === path ? { ...w, delete_originals: v } : w)));

  const toggleAutostart = async (v: boolean) => {
    try { await api.setAutostart(v); setAutostart(v); }
    catch (e) { showToast("Failed: " + e); }
  };
  const toggleStartMin = async (v: boolean) => {
    try { await api.setStartMinimized(v); setStartMin(v); }
    catch (e) { showToast("Failed: " + e); }
  };
  const toggleMinTray = async (v: boolean) => {
    try { await api.setMinimizeToTray(v); setMinTray(v); }
    catch (e) { showToast("Failed: " + e); }
  };
  const toggleAutoUpdate = async (v: boolean) => {
    try { await api.setAutoUpdate(v); setAutoUpdate(v); }
    catch (e) { showToast("Failed: " + e); }
  };
  const toggleConvertHeic = async (v: boolean) => {
    try { await api.setConvertHeicOnExport(v); setConvertHeic(v); }
    catch (e) { showToast("Failed: " + e); }
  };
  const checkForUpdate = async () => {
    setCheckingUpdate(true);
    try {
      const ver = await api.checkForUpdate();
      if (ver) {
        if (confirm(`Version ${ver} is available. Install it now and restart?`)) {
          await api.installUpdate(); // installs + restarts; never returns
        }
      } else {
        showToast("You're on the latest version");
      }
    } catch (e) { showToast("Update check failed: " + e); }
    finally { setCheckingUpdate(false); }
  };
  const toggleSyncEvery = async (v: boolean) => {
    try { await api.setSyncEverything(v); setSyncEvery(v); showToast(v ? "Syncing everything locally" : "Continuous sync off"); }
    catch (e) { showToast("Failed: " + e); }
  };
  const toggleAutoSync = async (v: boolean) => {
    try { await api.setAutoSync(v); setAutoSync(v); showToast(v ? "Automatic sync on" : "Automatic sync off"); }
    catch (e) { showToast("Failed: " + e); }
  };
  // Persist the interval on commit (blur/Enter), clamped to a sane range.
  const commitAutoSyncInterval = async (mins: number) => {
    const clamped = Math.min(1440, Math.max(1, Math.round(mins || 0)));
    setAutoSyncMins(clamped);
    try { await api.setAutoSyncInterval(clamped); }
    catch (e) { showToast("Failed: " + e); }
  };

  const onAutoUnlockToggle = async (v: boolean) => {
    if (!v) {
      try { await api.disableAutoUnlock(); setAutoUnlock(false); showToast("Auto-unlock disabled"); }
      catch (e) { showToast("Failed: " + e); }
      return;
    }
    setAuPrompt(true); // ask for the password before arming
  };
  const confirmAutoUnlock = async () => {
    try {
      let allowPlaintext = false;
      if (!secureStore.biometric && !secureStore.keyring) {
        const ok = window.confirm(
          "This device has no OS secure store (Windows Hello / Touch ID / system keyring).\n\n" +
          "To unlock automatically, your password will be saved to disk, encrypted with a key " +
          "that is itself stored IN PLAIN TEXT on this PC. Anyone with access to this computer " +
          "could recover your password and decrypt your photos.\n\nEnable anyway?"
        );
        if (!ok) return;
        allowPlaintext = true;
      }
      const r = await api.enableAutoUnlock(auPassword, allowPlaintext);
      setAutoUnlock(true); setAuPrompt(false); setAuPassword("");
      showToast(
        r.store === "biometric" ? "Auto-unlock enabled (Windows Hello / Touch ID)"
        : r.store === "keyring" ? "Auto-unlock enabled (system keyring)"
        : "Auto-unlock enabled (plaintext key)"
      );
    } catch (e) { showToast("Failed: " + e); }
  };

  const changeStorage = async () => {
    const dir = await pickFolder();
    if (!dir || dir === storagePath) return;
    if (!window.confirm(`Move your photo library to:\n${dir}\n\nApp settings stay in their current location. The app will pause during the move.`)) return;
    setMoving({ done: 0, total: 0 });
    try {
      await api.changeStoragePath(dir);
      setStoragePath(dir);
      // The session is kept alive across the move (re-opened at the new path),
      // so no re-login is needed. Refresh in case re-open failed.
      const s = await api.session();
      if (s.logged_in) showToast("Storage moved to " + dir);
      else { showToast("Storage moved — please unlock again"); setSession(null); }
    } catch (e) { showToast("Move failed: " + e); }
    finally { setMoving(null); }
  };
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
    showToast("Exporting… progress is shown in the sidebar");
    try {
      const r = await api.takeout(dir, false);
      showToast(`Takeout complete — ${r.written} files (${r.errors} errors)`);
    } catch (e) {
      showToast("Takeout failed: " + e);
    }
  };

  return (
    <>
      <div className="topbar">
        <h2>Settings</h2>
      </div>
      <nav className="settings-tabs">
        {SETTINGS_TABS.map((t) => (
          <button
            key={t.id}
            className={tab === t.id ? "active" : ""}
            onClick={() => setTab(t.id)}
          >
            {t.label}
          </button>
        ))}
      </nav>
      <div className="content">
        {tab === "account" && <>
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
          <h3>Session</h3>
          <div className="actionbar">
            <button onClick={async () => { await api.lock(); setSession(null); }}>Lock</button>
            <button onClick={() => setConfirmSignOut(true)}>Sign out</button>
          </div>
        </div>
        </>}

        {tab === "general" && <>
        <div className="settings-section">
          <h3>General</h3>
          <label className="opt-row">
            <input type="checkbox" checked={autostart} onChange={(e) => toggleAutostart(e.target.checked)} />
            <span>Start automatically when I sign in to this computer</span>
          </label>
          {autostart && (
            <label className="opt-row" style={{ marginLeft: 26 }}>
              <input type="checkbox" checked={startMin} onChange={(e) => toggleStartMin(e.target.checked)} />
              <span>
                Start in the tray
                <span className="muted" style={{ display: "block", fontSize: 12 }}>
                  Launch hidden in the background — the window stays closed until you open it from the tray icon.
                </span>
              </span>
            </label>
          )}
          <label className="opt-row">
            <input type="checkbox" checked={minTray} onChange={(e) => toggleMinTray(e.target.checked)} />
            <span>Minimize to the tray instead of quitting when I close the window</span>
          </label>
          <label className="opt-row">
            <input type="checkbox" checked={autoUpdate} onChange={(e) => toggleAutoUpdate(e.target.checked)} />
            <span>
              Automatically download updates
              <span className="muted" style={{ display: "block", fontSize: 12 }}>
                New versions are downloaded in the background and applied the next time you open the app.
                When off, you'll be offered the update in the sidebar instead.
              </span>
            </span>
          </label>
          <label className="opt-row">
            <input type="checkbox" checked={convertHeic} onChange={(e) => toggleConvertHeic(e.target.checked)} />
            <span>
              Convert HEIC to JPG on drag/copy
              <span className="muted" style={{ display: "block", fontSize: 12 }}>
                When you drag or copy photos out to other apps, HEIC images are converted to JPG first.
                Many apps can't open HEIC — leave this on to avoid errors. Your stored photos are unchanged.
              </span>
            </span>
          </label>
          <label className="opt-row">
            <input type="checkbox" checked={autoUnlock} onChange={(e) => onAutoUnlockToggle(e.target.checked)} />
            <span>
              Unlock automatically on startup
              <span className="muted" style={{ display: "block", fontSize: 12 }}>
                {secureStore.biometric
                  ? "Your password is encrypted with a key protected by Windows Hello / Touch ID."
                  : secureStore.keyring
                  ? "Your password is encrypted with a key kept in your system keyring (login Keychain / GNOME Keyring / KWallet)."
                  : "No secure store on this device — enabling will save a key in plain text (you'll be warned)."}
              </span>
            </span>
          </label>
          {auPrompt && (
            <div className="row" style={{ maxWidth: 360, marginTop: 8, gap: 8 }}>
              <input type="password" placeholder="Confirm your password" value={auPassword}
                onChange={(e) => setAuPassword(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && confirmAutoUnlock()} />
              <button onClick={confirmAutoUnlock} disabled={!auPassword}>Enable</button>
              <button onClick={() => { setAuPrompt(false); setAuPassword(""); }}>Cancel</button>
            </div>
          )}
        </div>
        </>}

        {tab === "sync" && <>
        <div className="settings-section">
          <h3>Sync</h3>
          <label className="opt-row">
            <input type="checkbox" checked={autoSync} onChange={(e) => toggleAutoSync(e.target.checked)} />
            <span>
              Sync automatically in the background
              <span className="muted" style={{ display: "block", fontSize: 12 }}>
                While idle, periodically pull new photos and push local changes.
              </span>
            </span>
          </label>
          <label
            style={{
              display: "flex",
              alignItems: "center",
              gap: 8,
              padding: "7px 0 7px 26px",
              opacity: autoSync ? 1 : 0.5,
            }}
          >
            <span>Sync every</span>
            <input
              type="number"
              min={1}
              max={1440}
              disabled={!autoSync}
              value={autoSyncMins}
              onChange={(e) => setAutoSyncMins(Number(e.target.value))}
              onBlur={(e) => commitAutoSyncInterval(Number(e.target.value))}
              onKeyDown={(e) => { if (e.key === "Enter") (e.target as HTMLInputElement).blur(); }}
              style={{ width: 56 }}
            />
            <span>minutes</span>
          </label>
          <label className="opt-row">
            <input type="checkbox" checked={syncEvery} onChange={(e) => toggleSyncEvery(e.target.checked)} />
            <span>
              Keep everything on this device
              <span className="muted" style={{ display: "block", fontSize: 12 }}>
                Continuously sync and download all original files so your whole library stays available offline.
              </span>
            </span>
          </label>
        </div>

        <div className="settings-section">
          <h3>Watch folders</h3>
          <p className="muted" style={{ fontSize: 13 }}>
            New photos and videos placed in these folders are imported automatically.
          </p>
          {watchFolders.length === 0 && (
            <p className="muted" style={{ fontSize: 13 }}>No folders watched yet.</p>
          )}
          {watchFolders.map((w) => (
            <div key={w.path} className="settings-section" style={{ padding: "10px 12px", marginBottom: 8 }}>
              <div className="row" style={{ alignItems: "center", justifyContent: "space-between", gap: 8 }}>
                <span style={{ wordBreak: "break-all" }}>{w.path}</span>
                <button onClick={() => removeWatchFolder(w.path)}>Remove</button>
              </div>
              <label className="opt-row" style={{ marginTop: 6 }}>
                <input
                  type="checkbox"
                  checked={w.delete_originals}
                  onChange={(e) => toggleDeleteOriginals(w.path, e.target.checked)}
                />
                <span>
                  Delete originals after successful import
                  <span className="muted" style={{ display: "block", fontSize: 12 }}>
                    Each original is <b>permanently deleted</b>, but only after its encrypted copy is
                    written, decrypts back to the exact same file, and is confirmed in your library.
                  </span>
                </span>
              </label>
            </div>
          ))}
          <button onClick={addWatchFolder}>Add folder…</button>
          {watchStatus && (
            <div className="muted" style={{ fontSize: 12, marginTop: 8 }}>{watchStatus}</div>
          )}
        </div>
        </>}

        {tab === "storage" && <>
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
          <h3>Storage location</h3>
          <p className="muted" style={{ fontSize: 13 }}>Where your encrypted photo library is kept. App settings stay in the app data folder.</p>
          <p className="muted" style={{ fontSize: 13, wordBreak: "break-all" }}>{storagePath}</p>
          <button onClick={changeStorage} disabled={!!moving}>Change…</button>
          {moving && (
            <div style={{ marginTop: 10, maxWidth: 360 }}>
              <div className="storage-bar">
                <div style={{ width: (moving.total > 0 ? Math.min(100, (moving.done / moving.total) * 100) : 0) + "%" }} />
              </div>
              <div className="muted" style={{ fontSize: 12, marginTop: 4 }}>
                Moving… {fmtMB(Math.round(moving.done / 1048576))} / {fmtMB(Math.round(moving.total / 1048576))}
              </div>
            </div>
          )}
        </div>

        <div className="settings-section">
          <h3>Takeout</h3>
          <p className="muted" style={{ fontSize: 13 }}>Download and decrypt your entire library to a folder.</p>
          <button onClick={doTakeout}>Export library…</button>
        </div>
        </>}

        {tab === "about" && <>
        <div className="settings-section">
          <h3>About</h3>
          <div className="muted">Stingle Desktop</div>
          {version && <div className="muted" style={{ fontSize: 13, marginTop: 4 }}>Version {version}</div>}
          <div className="actionbar" style={{ marginTop: 12 }}>
            <button onClick={checkForUpdate} disabled={checkingUpdate}>
              {checkingUpdate ? "Checking…" : "Check for updates"}
            </button>
          </div>
        </div>
        </>}
      </div>

      {confirmSignOut && (
        <ConfirmDialog
          title="Sign out"
          message={<>Do you want to delete all locally stored data (downloaded photos, thumbnails and database) for <b>{session.email}</b>? You can keep it for a faster next sign-in.</>}
          onClose={() => setConfirmSignOut(false)}
          actions={[
            { label: "Cancel", onClick: () => setConfirmSignOut(false) },
            { label: "Keep data", onClick: async () => { setConfirmSignOut(false); await api.logout(false); setSession(null); } },
            { label: "Delete all data", variant: "danger", onClick: async () => { setConfirmSignOut(false); await api.logout(true); setSession(null); } },
          ]}
        />
      )}
    </>
  );
}

/* ----------------------------- Actions ----------------------------- */

type MoveDest = { type: "gallery" } | { type: "album"; id: string };

function MoveDialog({ fromSet, fromAlbum, count, onPick, onClose }: {
  fromSet: number; fromAlbum: string | null; count: number;
  onPick: (dest: MoveDest, isMoving: boolean) => void; onClose: () => void;
}) {
  const [albums, setAlbums] = useState<Album[]>([]);
  // The core choice, in plain file-manager terms. Default to Move (keeps parity
  // with the mobile app and the previous desktop behavior).
  const [isMoving, setIsMoving] = useState(true);
  useEffect(() => {
    api.listAlbums().then(setAlbums);
    const h = (e: KeyboardEvent) => { if (e.key === "Escape") onClose(); };
    window.addEventListener("keydown", h);
    return () => window.removeEventListener("keydown", h);
  }, [onClose]);

  // You can add into your own albums, or a shared album where you have allowAdd.
  const targets = albums.filter((a) =>
    a.album_id !== fromAlbum &&
    (a.is_owner || (a.is_shared && parsePermissions(a.permissions).add)));
  const priv = targets.filter((a) => !a.is_shared);
  const shared = targets.filter((a) => a.is_shared);
  const verb = isMoving ? "Move" : "Copy";

  const newAlbum = async () => {
    const name = prompt("New album name:");
    if (!name) return;
    const id = await api.createAlbum(name);
    onPick({ type: "album", id }, isMoving);
  };

  const Card = (a: Album) => (
    <div key={a.album_id} className="move-card" onClick={() => onPick({ type: "album", id: a.album_id }, isMoving)}>
      <div className="move-cover">
        {a.cover === BLANK_COVER
          ? <span>🖼️</span>
          : a.cover ? <img src={mediaUrl(SET_ALBUM, a.cover, true, a.album_id)} /> : <span>📁</span>}
      </div>
      <div className="move-name" title={a.name}>{a.name}</div>
      <div className="move-count">{a.count} item{a.count === 1 ? "" : "s"}</div>
    </div>
  );

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal move-dialog" onClick={(e) => e.stopPropagation()}>
        <div className="move-head">
          <h3>{verb} {count} item{count === 1 ? "" : "s"} to…</h3>
          <button className="icon-btn" onClick={onClose}>✕</button>
        </div>

        <div className="seg" role="tablist">
          <button className={isMoving ? "active" : ""} aria-selected={isMoving} onClick={() => setIsMoving(true)}>Move</button>
          <button className={!isMoving ? "active" : ""} aria-selected={!isMoving} onClick={() => setIsMoving(false)}>Copy</button>
        </div>
        <p className="seg-hint">
          {isMoving
            ? "Adds them to the destination and removes them from their current location."
            : "Adds them to the destination and keeps them where they are now."}
        </p>

        <div className="move-grid">
          {fromSet === SET_ALBUM && (
            <div className="move-card" onClick={() => onPick({ type: "gallery" }, isMoving)}>
              <div className="move-cover special"><span>🖼️</span></div>
              <div className="move-name">Gallery</div>
              <div className="move-count">Main library</div>
            </div>
          )}
          <div className="move-card" onClick={newAlbum}>
            <div className="move-cover special"><span>＋</span></div>
            <div className="move-name">New album</div>
            <div className="move-count">Create &amp; {verb.toLowerCase()}</div>
          </div>
        </div>
        {priv.length > 0 && <><div className="move-section">My albums</div><div className="move-grid">{priv.map(Card)}</div></>}
        {shared.length > 0 && <><div className="move-section">Shared albums</div><div className="move-grid">{shared.map(Card)}</div></>}
      </div>
    </div>
  );
}

type ConfirmAction = { label: string; onClick: () => void; variant?: "primary" | "danger" };

// A small reusable confirmation modal supporting up to a few labeled actions.
// Esc or a backdrop click cancels (runs `onClose`).
function ConfirmDialog({ title, message, actions, onClose }: {
  title: string; message?: React.ReactNode; actions: ConfirmAction[]; onClose: () => void;
}) {
  useEffect(() => {
    const h = (e: KeyboardEvent) => { if (e.key === "Escape") onClose(); };
    window.addEventListener("keydown", h);
    return () => window.removeEventListener("keydown", h);
  }, [onClose]);

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3>{title}</h3>
        {message && <p className="muted" style={{ fontSize: 14, lineHeight: 1.5, marginTop: 0 }}>{message}</p>}
        <div className="actionbar" style={{ justifyContent: "flex-end", marginTop: 18 }}>
          {actions.map((a, i) => (
            <button
              key={i}
              className={a.variant === "primary" ? "primary" : ""}
              style={a.variant === "danger" ? { background: "#b3261e", color: "#fff", borderColor: "#b3261e" } : undefined}
              onClick={a.onClick}
            >
              {a.label}
            </button>
          ))}
        </div>
      </div>
    </div>
  );
}

/** Per-album capability flags. Omitted (gallery/trash) ⇒ everything allowed. */
type FileCaps = { canCopy: boolean; canDelete: boolean; canShare: boolean };
const FULL_CAPS: FileCaps = { canCopy: true, canDelete: true, canShare: true };

function ActionButtons({ set, albumId, filenames, onDone, showToast, onTrashed, caps = FULL_CAPS }: {
  set: number; albumId: string | null; filenames: string[];
  onDone: () => void; showToast: (m: string) => void;
  onTrashed?: (filenames: string[]) => void;
  caps?: FileCaps;
}) {
  const [moving, setMoving] = useState(false);
  const [sharing, setSharing] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const save = async () => {
    const dir = await pickFolder();
    if (!dir) return;
    showToast("Saving…");
    const n = await api.saveFiles(set, albumId, filenames, dir);
    showToast(`Saved ${n} file${n === 1 ? "" : "s"}`);
  };
  const del = async () => {
    setConfirmDelete(false);
    await api.trashCtx(set, albumId, filenames);
    onTrashed?.(filenames); // optimistic: remove from the grid right away
    showToast("Moved to trash");
    onDone();
  };
  const move = async (dest: MoveDest, isMoving: boolean) => {
    setMoving(false);
    try {
      if (dest.type === "gallery") await api.moveToGallery(albumId!, filenames, isMoving);
      else await api.moveToAlbum(set, albumId, filenames, dest.id, isMoving);
      showToast(isMoving ? "Moved" : "Copied");
      onDone();
    } catch (e) { showToast((isMoving ? "Move" : "Copy") + " failed: " + e); }
  };
  return (
    <>
      {caps.canShare && <button onClick={() => setSharing(true)}>👥 Share</button>}
      {caps.canCopy && <button onClick={save}>⤓ Save</button>}
      {caps.canCopy && <button onClick={() => setMoving(true)}>→ Move</button>}
      {caps.canDelete && <button onClick={() => setConfirmDelete(true)}>🗑️ Delete</button>}
      {moving && <MoveDialog fromSet={set} fromAlbum={albumId} count={filenames.length} onPick={move} onClose={() => setMoving(false)} />}
      {sharing && (
        <ShareFilesDialog set={set} albumId={albumId} filenames={filenames}
          onClose={() => setSharing(false)}
          onShared={() => { setSharing(false); onDone(); }}
          showToast={showToast} />
      )}
      {confirmDelete && (
        <ConfirmDialog
          title={filenames.length === 1 ? "Move to trash?" : `Move ${filenames.length} items to trash?`}
          message="You can restore items from Trash later."
          onClose={() => setConfirmDelete(false)}
          actions={[
            { label: "Cancel", onClick: () => setConfirmDelete(false) },
            { label: "Move to trash", variant: "danger", onClick: del },
          ]}
        />
      )}
    </>
  );
}

/* ----------------------------- Viewer ----------------------------- */

function Viewer({ items, index, set, albumId, onClose, onChanged, showToast, onTrashed, caps = FULL_CAPS }: {
  items: FileItem[]; index: number; set: number; albumId: string | null;
  onClose: () => void; onChanged: () => void; showToast: (m: string) => void;
  onTrashed?: (filenames: string[]) => void;
  caps?: FileCaps;
}) {
  const [i, setI] = useState(index);
  const vidPress = useRef<{ x: number; y: number } | null>(null);
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

  // Ctrl/Cmd+C copies the currently-viewed item — blocked without copy permission.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.ctrlKey || e.metaKey) || e.key.toLowerCase() !== "c") return;
      if (inTextField(e) || !f || !caps.canCopy) return;
      copyToClipboard(set, albumId, [f.filename], showToast);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [f, set, albumId, showToast, caps.canCopy]);

  // Warm the encrypted on-disk cache for the neighbors so next/back doesn't
  // stall on a full network download (most of a large gallery is remote-only).
  // Slightly debounced so holding an arrow key doesn't queue a fetch per step.
  useEffect(() => {
    const neighbors = [i + 1, i - 1, i + 2]
      .filter((j) => j >= 0 && j < items.length && j !== i)
      .map((j) => items[j].filename);
    if (neighbors.length === 0) return;
    const t = setTimeout(() => { api.prefetchMedia(set, neighbors).catch(() => {}); }, 150);
    return () => clearTimeout(t);
  }, [i, items, set]);

  if (!f) return null;
  // `is_video` already comes with the listed row (decoded from its header), so use
  // it directly — no extra IPC round-trip to gate the viewer. This lets the image
  // (and its instant cached thumbnail) mount the moment you click, instead of
  // waiting on a command that can be starved right after a big scroll.
  const isVid = f.is_video;
  const thumbUrl = mediaUrl(set, f.filename, true, albumId);
  const fullUrl = mediaUrl(set, f.filename, false, albumId);
  const stop = (e: React.MouseEvent) => e.stopPropagation();
  return (
    <div className="viewer" onClick={onClose}>
      <div className="close">✕</div>
      <div className="viewer-actions" onClick={stop}>
        <ActionButtons set={set} albumId={albumId} filenames={[f.filename]}
          caps={caps} onTrashed={onTrashed}
          onDone={() => { onChanged(); onClose(); }} showToast={showToast} />
      </div>
      {i > 0 && <button className="nav-btn prev" onClick={(e) => { e.stopPropagation(); setI(i - 1); }}>‹</button>}
      {isVid ? <video src={videoUrl(set, f.filename, albumId)} controls autoPlay onClick={stop}
            onMouseDown={(e) => {
              // Start a drag only from the video frame, not the bottom controls bar.
              const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
              vidPress.current = e.clientY < r.bottom - 56 ? { x: e.clientX, y: e.clientY } : null;
            }}
            onMouseMove={(e) => {
              const p = vidPress.current;
              if (p && Math.hypot(e.clientX - p.x, e.clientY - p.y) >= 6) {
                vidPress.current = null;
                if (caps.canCopy) nativeDragOut(set, albumId, [f.filename]);
              }
            }}
            onMouseUp={() => { vidPress.current = null; }} />
        : <ZoomableImage key={f.filename} thumbUrl={thumbUrl} fullUrl={fullUrl}
            onDragOut={caps.canCopy ? () => nativeDragOut(set, albumId, [f.filename]) : undefined} />}
      {i < items.length - 1 && <button className="nav-btn next" onClick={(e) => { e.stopPropagation(); setI(i + 1); }}>›</button>}
      <div className="name">{i + 1} / {items.length}</div>
    </div>
  );
}
