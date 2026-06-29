import React, { useEffect, useLayoutEffect, useState, useCallback, useRef, useMemo } from "react";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { startDrag } from "@crabnebula/tauri-plugin-drag";
import {
  api, mediaUrl, pickFiles, pickFolder,
  Session, FileItem, Album, LocalAccount, WatchFolder,
  SET_GALLERY, SET_TRASH, SET_ALBUM, BLANK_COVER,
} from "./api";
import logoUrl from "./assets/stingle-logo.png";

type View = "gallery" | "albums" | "trash" | "settings";

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
const SettingsIcon = () => (
  <svg {...ICON_PROPS}>
    <circle cx="12" cy="12" r="3.1" />
    <path d="M19.4 13a1.6 1.6 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.6 1.6 0 0 0-2.7 1.1V19a2 2 0 1 1-4 0v-.1a1.6 1.6 0 0 0-2.7-1.1l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1A1.6 1.6 0 0 0 4.6 13a2 2 0 1 1 0-4 1.6 1.6 0 0 0 1.1-2.7l-.1-.1A2 2 0 1 1 8.4 3.4l.1.1A1.6 1.6 0 0 0 11 4.6 2 2 0 1 1 15 4.6a1.6 1.6 0 0 0 2.7 1.1l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1A1.6 1.6 0 0 0 19.4 11a2 2 0 1 1 0 4z" />
  </svg>
);

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

/** Start a native OS drag of one or more library items out to other apps
 *  (Explorer, Telegram, …). Files are decrypted to a temp folder for the drag
 *  and cleaned up when it ends. Returns immediately if the export fails. */
async function nativeDragOut(set: number, albumId: string | null, filenames: string[]) {
  if (filenames.length === 0) return;
  try {
    const exp = await api.exportForDrag(set, albumId, filenames);
    const cleanup = exp.icon ? [...exp.files, exp.icon] : exp.files;
    await startDrag(
      { item: exp.files, icon: exp.icon, mode: "copy" },
      () => { api.cleanupDragExport(cleanup); }
    );
  } catch { /* drag export failed — ignore */ }
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

/** One thumbnail. Memoized so a selection change (or a sync-triggered reload)
 *  only re-renders the tiles that actually changed — critical with thousands of
 *  tiles, where re-rendering the whole grid on every marquee frame would jank. */
/** A shared IntersectionObserver for the whole grid: each tile registers its
 *  element + a setter, and is told when it enters/leaves a band ~2 viewports tall
 *  around the scroll viewport. One observer for the whole grid is O(visible) work
 *  per scroll, not O(items) — so a 10k-photo grid stays cheap. The band (rootMargin)
 *  is generous enough that tiles reload before they scroll back into view, so there
 *  is no blank flash on normal scrolling. */
function useTileVisibility(): (el: Element, cb: (visible: boolean) => void) => () => void {
  const cbs = useRef<Map<Element, (v: boolean) => void>>(new Map()).current;
  const ioRef = useRef<IntersectionObserver | null>(null);

  const register = useCallback((el: Element, cb: (visible: boolean) => void) => {
    if (!ioRef.current) {
      // Resolve the scroll container from a real tile (it's an ancestor and is
      // attached by the time a ref callback runs); fall back to the viewport.
      const root = (el.closest(".content") as Element | null) ?? null;
      ioRef.current = new IntersectionObserver(
        (entries) => {
          for (const e of entries) {
            const setter = cbs.get(e.target);
            if (setter) setter(e.isIntersecting);
          }
        },
        { root, rootMargin: "200% 0px", threshold: 0 },
      );
    }
    cbs.set(el, cb);
    ioRef.current.observe(el);
    return () => {
      ioRef.current?.unobserve(el);
      cbs.delete(el);
    };
  }, [cbs]);

  useEffect(() => () => { ioRef.current?.disconnect(); cbs.clear(); }, [cbs]);

  return register;
}

const TileView = React.memo(function TileView({
  f, set, albumId, selected, selectionEmpty, renderExtra, register,
}: {
  f: FileItem; set: number; albumId: string | null;
  selected: boolean; selectionEmpty: boolean;
  renderExtra?: (f: FileItem) => React.ReactNode;
  // Registers this tile with the grid's shared IntersectionObserver; the callback
  // flips whether the <img> is mounted as the tile enters/leaves the load band.
  register: (el: Element, cb: (visible: boolean) => void) => () => void;
}) {
  const tileRef = useRef<HTMLDivElement>(null);
  // Only mount the <img> while the tile is within the observer's band. Removing it
  // when far off-screen ABORTS any in-flight stingle:// request — the one thing
  // native loading="lazy" can't do — so a fast scroll never builds a backlog of
  // stale requests that starve the now-visible thumbnails.
  const [show, setShow] = useState(false);
  useEffect(() => {
    const el = tileRef.current;
    if (!el) return;
    return register(el, setShow);
  }, [register]);
  return (
    <div ref={tileRef} data-fn={f.filename} className={"tile" + (selected ? " sel" : "")}>
      {show && <img draggable={false} src={mediaUrl(set, f.filename, true, albumId)} />}
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
            // just this tile (and make it the selection, like Explorer).
            let files: string[];
            if (st.sel.has(fn) && st.sel.size > 0) {
              files = [...st.sel];
            } else {
              files = [fn];
              setSel(new Set([fn]));
            }
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
  const register = useTileVisibility();
  const Tile = (f: FileItem) => (
    <TileView key={f.filename} f={f} set={set} albumId={albumId}
      selected={sel.has(f.filename)} selectionEmpty={selectionEmpty} renderExtra={renderExtra}
      register={register} />
  );

  // Re-grouping the whole list is cheap but pointless on every render (e.g. each
  // marquee frame), so cache it until the items array itself changes.
  const groups = useMemo(() => (grouped ? groupByDate(items) : null), [grouped, items]);

  return (
    <div ref={wrapRef} className="grid-wrap" onMouseDown={startDrag}>
      {groups
        ? groups.map((g) => (
            <div key={g.label}>
              <div className="date-header">{g.label}</div>
              <div className="grid">{g.entries.map(({ f }) => Tile(f))}</div>
            </div>
          ))
        : <div className="grid">{items.map((f) => Tile(f))}</div>}
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
  // Whether biometric auto-unlock is set up, so we can offer a manual retry.
  // The startup auto-unlock attempt prompts once; if the user cancels it (or it
  // otherwise fails) this button is the only way back in short of restarting.
  const [canBiometric, setCanBiometric] = useState(false);

  useEffect(() => {
    api.lastAccount().then((a) => { setLastAcc(a); setReady(true); }).catch(() => setReady(true));
  }, []);

  useEffect(() => {
    (async () => {
      try {
        const [enabled, store] = await Promise.all([
          api.isAutoUnlockEnabled().catch(() => false),
          api.secureStoreStatus().catch(() => ({ biometric: false })),
        ]);
        setCanBiometric(enabled && store.biometric);
      } catch { /* leave the button hidden */ }
    })();
  }, []);

  // Retry biometric unlock from the login screen (re-triggers the OS prompt).
  // Only meaningful for an offline resume — a server-expired token can't be
  // revived this way, so the button is hidden when sessionExpired.
  const biometricUnlock = async () => {
    setBusy(true); setError("");
    try {
      const u = await api.tryAutoUnlock();
      if (u.logged_in) { onAuthed(u); return; }
      setError("Biometric unlock was canceled.");
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
          {canBiometric && !sessionExpired && (
            <button style={{ width: "100%", marginTop: 8 }} disabled={busy} onClick={biometricUnlock}>
              Unlock with biometrics
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

// One phase row: an icon, a label, a done/total count, and a progress bar.
function PhaseRow({ icon, label, value }: { icon: string; label: string; value: { done: number; total: number } }) {
  const pct = value.total > 0 ? Math.min(100, (value.done / value.total) * 100) : 0;
  return (
    <div className="phase-row">
      <span className="phase-icon">{icon}</span>
      <span className="phase-label">{label}</span>
      <span className="phase-count">{value.done} / {value.total}</span>
      <div className="sync-bar"><div style={{ width: pct + "%" }} /></div>
    </div>
  );
}

// Consolidated sync status: a calm "Up to date" when idle, or an animated header
// plus one progress row per active phase (upload / thumbnails / cache).
function SyncPanel({ syncing, upload, thumbs, originals }: {
  syncing: boolean; upload: Progress; thumbs: Progress; originals: Progress;
}) {
  const up = upload && upload.total > 0 ? upload : null;
  const th = thumbs && thumbs.total > 0 ? thumbs : null;
  const or = originals && originals.total > 0 ? originals : null;
  const active = syncing || !!up || !!th || !!or;

  return (
    <div className="sync-panel">
      <div className="sync-head">
        {active ? (
          <><span className="spinner" /> <span>Syncing…</span></>
        ) : (
          <span className="sync-idle">✓ Up to date</span>
        )}
      </div>
      {up && <PhaseRow icon="↑" label="Uploading" value={up} />}
      {th && <PhaseRow icon="⤓" label="Thumbnails" value={th} />}
      {or && <PhaseRow icon="⤓" label="Cache" value={or} />}
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
  // Set when auto-update is off and the backend reports an available update.
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

  // Auto-update off: the backend emits `update-available` at startup if a newer
  // version exists, so the sidebar can offer a one-click install.
  useEffect(() => {
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
    return () => {
      if (reloadTimer) clearTimeout(reloadTimer);
      u0.then((f) => f()); u0b.then((f) => f());
      u1.then((f) => f()); u2.then((f) => f()); u3.then((f) => f()); u4.then((f) => f());
    };
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

  // Drag files from the OS into the app → import them (encrypted on the way in).
  const [dropActive, setDropActive] = useState(false);
  useEffect(() => {
    let un: (() => void) | undefined;
    let cancelled = false;
    getCurrentWebview().onDragDropEvent((event) => {
      const p = event.payload as { type: string; paths?: string[] };
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

  const pct = session.space_quota > 0 ? Math.min(100, (session.space_used / session.space_quota) * 100) : 0;

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
          <button className={view === "trash" ? "active" : ""} onClick={() => setView("trash")}><TrashIcon /> Trash</button>
          <button className={view === "settings" ? "active" : ""} onClick={() => setView("settings")}><SettingsIcon /> Settings</button>
        </div>
        <div className="spacer" />
        {updateVer && (
          <button className="update-banner" disabled={updating} onClick={installUpdate}>
            {updating ? "Updating…" : `⤓ Update to ${updateVer} — restart now`}
          </button>
        )}
        <SyncPanel syncing={syncing} upload={upload} thumbs={thumbs} originals={originals} />
        <div className="side-div" />
        <div className="acct">{session.email}</div>
        <div className="storage-bar"><div style={{ width: pct + "%" }} /></div>
        <div className="acct">{fmtMB(session.space_used)} / {fmtMB(session.space_quota)}</div>
      </div>

      <div className="main">
        {view === "gallery" && <GalleryView reloadSignal={reloadKey} syncing={syncing} onSync={doSync} showToast={showToast} onChanged={reload} />}
        {view === "albums" && <AlbumsView reloadSignal={reloadKey} showToast={showToast} />}
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
): { items: FileItem[]; reload: () => void } {
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
  return { items, reload };
}

function GalleryView({ syncing, onSync, showToast, onChanged, reloadSignal }: {
  syncing: boolean; onSync: () => void; showToast: (m: string) => void; onChanged: () => void; reloadSignal: number;
}) {
  const [sel, setSel] = useState<Set<string>>(new Set());
  const [viewerIdx, setViewerIdx] = useState<number | null>(null);

  const fetchPage = useCallback(
    (offset: number, limit: number) => api.listGallery(offset, limit), []);
  const { items, reload: load } = usePagedFiles(fetchPage, reloadSignal);

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
            sel={sel} setSel={setSel} onOpen={setViewerIdx} showToast={showToast} />
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
                onDone={() => { clearSel(); load(); }} showToast={showToast} />
              <button onClick={clearSel}>Cancel</button>
            </>
          ) : (
            <>
              {album.is_owner && <button onClick={addFiles}>＋ Add</button>}
              {album.is_owner && <button onClick={share}>👥 Share</button>}
              {album.is_owner && album.is_shared && <button onClick={unshare}>Unshare</button>}
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
            sel={sel} setSel={setSel} showToast={showToast}
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

  // App options
  const [autostart, setAutostart] = useState(false);
  const [minTray, setMinTray] = useState(false);
  const [autoUpdate, setAutoUpdate] = useState(true);
  const [syncEvery, setSyncEvery] = useState(false);
  const [autoUnlock, setAutoUnlock] = useState(false);
  const [biometric, setBiometric] = useState(false);
  const [auPrompt, setAuPrompt] = useState(false);
  const [auPassword, setAuPassword] = useState("");
  const [storagePath, setStoragePath] = useState("");
  const [moving, setMoving] = useState<{ done: number; total: number } | null>(null);
  const [confirmSignOut, setConfirmSignOut] = useState(false);

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
    api.getMinimizeToTray().then(setMinTray).catch(() => {});
    api.getAutoUpdate().then(setAutoUpdate).catch(() => {});
    api.getAppVersion().then(setVersion).catch(() => {});
    api.getSyncEverything().then(setSyncEvery).catch(() => {});
    api.isAutoUnlockEnabled().then(setAutoUnlock).catch(() => {});
    api.secureStoreStatus().then((s) => setBiometric(s.biometric)).catch(() => {});
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
  const toggleMinTray = async (v: boolean) => {
    try { await api.setMinimizeToTray(v); setMinTray(v); }
    catch (e) { showToast("Failed: " + e); }
  };
  const toggleAutoUpdate = async (v: boolean) => {
    try { await api.setAutoUpdate(v); setAutoUpdate(v); }
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
      if (!biometric) {
        const ok = window.confirm(
          "This device has no biometric secure store (Windows Hello / Touch ID).\n\n" +
          "To unlock automatically, your password will be saved to disk, encrypted with a key " +
          "that is itself stored IN PLAIN TEXT on this PC. Anyone with access to this computer " +
          "could recover your password and decrypt your photos.\n\nEnable anyway?"
        );
        if (!ok) return;
        allowPlaintext = true;
      }
      const r = await api.enableAutoUnlock(auPassword, allowPlaintext);
      setAutoUnlock(true); setAuPrompt(false); setAuPassword("");
      showToast(r.used_plaintext ? "Auto-unlock enabled (plaintext key)" : "Auto-unlock enabled");
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
          <h3>General</h3>
          <label className="opt-row">
            <input type="checkbox" checked={autostart} onChange={(e) => toggleAutostart(e.target.checked)} />
            <span>Start automatically when I sign in to this computer</span>
          </label>
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
            <input type="checkbox" checked={autoUnlock} onChange={(e) => onAutoUnlockToggle(e.target.checked)} />
            <span>
              Unlock automatically on startup
              <span className="muted" style={{ display: "block", fontSize: 12 }}>
                {biometric
                  ? "Your password is encrypted with a key protected by Windows Hello / Touch ID."
                  : "No biometric store on this device — enabling will save a key in plain text (you'll be warned)."}
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

        <div className="settings-section">
          <h3>Sync</h3>
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

        <div className="settings-section">
          <h3>About</h3>
          <div className="row" style={{ alignItems: "center", justifyContent: "space-between", gap: 8, maxWidth: 360 }}>
            <span className="muted">Stingle Desktop{version && ` — version ${version}`}</span>
            <button onClick={checkForUpdate} disabled={checkingUpdate}>
              {checkingUpdate ? "Checking…" : "Check for updates"}
            </button>
          </div>
        </div>

        <div className="settings-section">
          <h3>Session</h3>
          <div className="actionbar">
            <button onClick={async () => { await api.lock(); setSession(null); }}>Lock</button>
            <button onClick={() => setConfirmSignOut(true)}>Sign out</button>
          </div>
        </div>
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

  const targets = albums.filter((a) => a.album_id !== fromAlbum && a.is_owner);
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

  // Ctrl/Cmd+C copies the currently-viewed item.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.ctrlKey || e.metaKey) || e.key.toLowerCase() !== "c") return;
      if (inTextField(e) || !f) return;
      copyToClipboard(set, albumId, [f.filename], showToast);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [f, set, albumId, showToast]);

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
          onDone={() => { onChanged(); onClose(); }} showToast={showToast} />
      </div>
      {i > 0 && <button className="nav-btn prev" onClick={(e) => { e.stopPropagation(); setI(i - 1); }}>‹</button>}
      {isVid ? <video src={fullUrl} controls autoPlay onClick={stop}
            onMouseDown={(e) => {
              // Start a drag only from the video frame, not the bottom controls bar.
              const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
              vidPress.current = e.clientY < r.bottom - 56 ? { x: e.clientX, y: e.clientY } : null;
            }}
            onMouseMove={(e) => {
              const p = vidPress.current;
              if (p && Math.hypot(e.clientX - p.x, e.clientY - p.y) >= 6) {
                vidPress.current = null;
                nativeDragOut(set, albumId, [f.filename]);
              }
            }}
            onMouseUp={() => { vidPress.current = null; }} />
        : <ZoomableImage key={f.filename} thumbUrl={thumbUrl} fullUrl={fullUrl}
            onDragOut={() => nativeDragOut(set, albumId, [f.filename])} />}
      {i < items.length - 1 && <button className="nav-btn next" onClick={(e) => { e.stopPropagation(); setI(i + 1); }}>›</button>}
      <div className="name">{i + 1} / {items.length}</div>
    </div>
  );
}
