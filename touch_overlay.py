"""Click-through Windows touch-position overlay."""

from __future__ import annotations

import ctypes
import queue
import time
import tkinter as tk
from dataclasses import dataclass
from typing import Callable, Optional


TRANSPARENT = "#010203"
SURFACE = "#10131c"
GRID = "#294651"
ACCENT = "#42d9f5"
TEXT = "#d6f3f7"
OUTSIDE = "#ff806f"
TOUCH_COLORS = (
    "#42d9f5",
    "#89e66f",
    "#ffc766",
    "#c99aff",
    "#ff8fbd",
    "#62e2c2",
    "#8fb7ff",
    "#f0df7d",
    "#ee9b73",
    "#8de2ee",
)

GWL_EXSTYLE = -20
WS_EX_TRANSPARENT = 0x00000020
WS_EX_TOOLWINDOW = 0x00000080
WS_EX_LAYERED = 0x00080000
WS_EX_NOACTIVATE = 0x08000000
SWP_NOMOVE = 0x0002
SWP_NOSIZE = 0x0001
SWP_NOACTIVATE = 0x0010
HWND_TOPMOST = -1
SPI_GETCLIENTAREAANIMATION = 0x1042
MIN_OVERLAY_WIDTH = 320
MIN_OVERLAY_HEIGHT = 100
RESIZE_EDGE_PX = 12
RESIZE_CORNER_PX = 28


def resize_geometry(
    edge: str,
    start: tuple[int, int, int, int],
    dx: int,
    dy: int,
) -> tuple[int, int, int, int]:
    """Resize a rectangle while keeping the opposite edges fixed."""
    x, y, width, height = start
    right = x + width
    bottom = y + height

    if "w" in edge:
        x = min(x + dx, right - MIN_OVERLAY_WIDTH)
        width = right - x
    elif "e" in edge:
        width = max(MIN_OVERLAY_WIDTH, width + dx)

    if "n" in edge:
        y = min(y + dy, bottom - MIN_OVERLAY_HEIGHT)
        height = bottom - y
    elif "s" in edge:
        height = max(MIN_OVERLAY_HEIGHT, height + dy)

    return x, y, width, height


@dataclass
class Dot:
    pointer_id: int
    x: float
    y: float
    inside: bool
    released_at: Optional[float] = None


class TouchOverlay:
    HOTKEY_POLL_MS = 40
    RELEASE_FADE_SECONDS = 0.11

    def __init__(
        self,
        lane_count: int,
        config: dict,
        save_config: Callable[[dict], None],
        start_in_edit_mode: bool = False,
    ) -> None:
        self.lane_count = lane_count
        self.config = config
        self.save_config = save_config
        self.events: queue.SimpleQueue = queue.SimpleQueue()
        self.dots: dict[int, Dot] = {}
        self.status = "Looking for phone…"
        self.connected = False
        self.running = True
        self._hotkey_down = False
        self._quit_hotkey_down = False
        self._drag_mode: Optional[str] = None
        self._drag_start = (0, 0, 0, 0, 0, 0)
        self._edit_button: Optional[tk.Toplevel] = None

        self.root = tk.Tk()
        self.root.title("Holodori Touch Overlay")
        self.root.overrideredirect(True)
        self.root.attributes("-topmost", True)
        self.root.configure(bg=TRANSPARENT)
        self.root.wm_attributes("-transparentcolor", TRANSPARENT)

        screen_w = self.root.winfo_screenwidth()
        screen_h = self.root.winfo_screenheight()
        default = {
            "x": int(screen_w * 0.10),
            "y": int(screen_h * 0.70),
            "w": int(screen_w * 0.80),
            "h": int(screen_h * 0.20),
            "configured": False,
        }
        for key, value in default.items():
            self.config.setdefault(key, value)
        self._apply_geometry()

        self.canvas = tk.Canvas(
            self.root,
            bg=TRANSPARENT,
            highlightthickness=0,
            bd=0,
        )
        self.canvas.pack(fill="both", expand=True)
        self.canvas.bind("<ButtonPress-1>", self._mouse_down)
        self.canvas.bind("<B1-Motion>", self._mouse_move)
        self.canvas.bind("<ButtonRelease-1>", self._mouse_up)
        self.canvas.bind("<Motion>", self._mouse_hover)
        self.canvas.bind("<Leave>", lambda _event: self.canvas.configure(cursor=""))
        self.root.bind("<Return>", lambda _event: self.set_edit_mode(False))
        self.root.bind("<Escape>", lambda _event: self.close())

        enabled = ctypes.c_int(1)
        try:
            ctypes.windll.user32.SystemParametersInfoW(
                SPI_GETCLIENTAREAANIMATION,
                0,
                ctypes.byref(enabled),
                0,
            )
        except Exception:
            pass
        self.motion_enabled = bool(enabled.value)

        self.edit_mode = bool(
            start_in_edit_mode or not self.config.get("configured", False)
        )
        self.root.update_idletasks()
        self._apply_window_style()
        self._create_edit_button()
        self._sync_edit_button()
        self._draw_static()
        self.root.after(8, self._frame)
        self.root.after(self.HOTKEY_POLL_MS, self._poll_hotkeys)

    def _apply_geometry(self) -> None:
        self.config["w"] = max(MIN_OVERLAY_WIDTH, int(self.config["w"]))
        self.config["h"] = max(MIN_OVERLAY_HEIGHT, int(self.config["h"]))
        x = int(self.config["x"])
        y = int(self.config["y"])
        self.root.geometry(
            f'{self.config["w"]}x{self.config["h"]}'
            f"{x:+d}{y:+d}"
        )
        self._position_edit_button()

    def _create_edit_button(self) -> None:
        button_window = tk.Toplevel(self.root)
        button_window.withdraw()
        button_window.overrideredirect(True)
        button_window.attributes("-topmost", True)
        button_window.configure(bg=SURFACE)
        button = tk.Button(
            button_window,
            text="Edit zone   Ctrl+Shift+O",
            command=lambda: self.set_edit_mode(True),
            bg=SURFACE,
            fg=TEXT,
            activebackground="#1b2730",
            activeforeground=ACCENT,
            bd=1,
            relief="solid",
            highlightthickness=0,
            padx=10,
            pady=5,
            cursor="hand2",
            font=("Segoe UI Semibold", 9),
        )
        button.pack()
        self._edit_button = button_window
        self._position_edit_button()

    def _position_edit_button(self) -> None:
        if self._edit_button is None:
            return
        self._edit_button.update_idletasks()
        button_w = self._edit_button.winfo_reqwidth()
        button_h = self._edit_button.winfo_reqheight()
        x = int(self.config.get("x", 0)) + int(self.config.get("w", 0)) - button_w
        y = int(self.config.get("y", 0)) - button_h - 6
        if y < 0:
            y = int(self.config.get("y", 0)) + 6
        self._edit_button.geometry(f"{button_w}x{button_h}{x:+d}{y:+d}")

    def _sync_edit_button(self) -> None:
        if self._edit_button is None:
            return
        if self.edit_mode:
            self._edit_button.withdraw()
        else:
            self._position_edit_button()
            self._edit_button.deiconify()
            self._edit_button.lift()

    def _apply_window_style(self) -> None:
        self.root.update_idletasks()
        user32 = ctypes.windll.user32
        tk_hwnd = self.root.winfo_id()
        # Tk creates a native wrapper around the drawable child returned by
        # winfo_id(). Extended top-level styles belong on that wrapper.
        hwnd = user32.GetParent(tk_hwnd) or tk_hwnd
        style = user32.GetWindowLongW(hwnd, GWL_EXSTYLE)
        style |= WS_EX_LAYERED | WS_EX_TOOLWINDOW
        if self.edit_mode:
            style &= ~(WS_EX_TRANSPARENT | WS_EX_NOACTIVATE)
        else:
            style |= WS_EX_TRANSPARENT | WS_EX_NOACTIVATE
        user32.SetWindowLongW(hwnd, GWL_EXSTYLE, style)
        user32.SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )

    def set_edit_mode(self, enabled: bool) -> None:
        self.edit_mode = enabled
        if not enabled:
            self.config["configured"] = True
            self.save_config(self.config)
        self._apply_window_style()
        self._sync_edit_button()
        self._draw_static()

    def publish_touch(
        self,
        pointer_id: int,
        x: float,
        y: float,
        inside: bool,
        released: bool,
    ) -> None:
        self.events.put(("touch", pointer_id, x, y, inside, released))

    def publish_cancel(self) -> None:
        self.events.put(("cancel",))

    def publish_status(self, text: str, connected: bool) -> None:
        self.events.put(("status", text, connected))

    def _draw_static(self) -> None:
        self.canvas.delete("static")
        width = max(1, self.root.winfo_width())
        height = max(1, self.root.winfo_height())
        if self.edit_mode:
            self.canvas.create_rectangle(
                0,
                0,
                width,
                height,
                fill=SURFACE,
                outline=ACCENT,
                width=2,
                tags="static",
            )
        else:
            self.canvas.create_rectangle(
                1,
                1,
                width - 2,
                height - 2,
                outline=GRID,
                width=1,
                tags="static",
            )

        for lane in range(1, self.lane_count):
            x = round(width * lane / self.lane_count)
            self.canvas.create_line(
                x, 0, x, height, fill=GRID, width=1, tags="static"
            )

        if self.edit_mode:
            self.canvas.create_text(
                18,
                18,
                anchor="nw",
                fill=TEXT,
                font=("Segoe UI Semibold", 13),
                text="PC TOUCH ZONE",
                tags="static",
            )
            self.canvas.create_text(
                18,
                44,
                anchor="nw",
                fill="#8fa9af",
                font=("Segoe UI", 10),
                text=(
                    "Drag to move  •  Drag any edge or corner to resize  •  "
                    "Enter: play"
                ),
                tags="static",
            )
            handle = 7
            for hx, hy in (
                (0, 0),
                (width - handle, 0),
                (0, height - handle),
                (width - handle, height - handle),
            ):
                self.canvas.create_rectangle(
                    hx,
                    hy,
                    hx + handle,
                    hy + handle,
                    fill=ACCENT,
                    outline="",
                    tags="static",
                )

        self.canvas.create_text(
            width - 10,
            8,
            anchor="ne",
            fill=ACCENT if self.connected else "#819298",
            font=("Segoe UI", 9),
            text=self.status,
            tags="static",
        )

    def _frame(self) -> None:
        if not self.running:
            return
        redraw_static = False
        while True:
            try:
                event = self.events.get_nowait()
            except queue.Empty:
                break
            if event[0] == "touch":
                _, pointer_id, x, y, inside, released = event
                dot = self.dots.get(pointer_id)
                if dot is None:
                    dot = Dot(pointer_id, x, y, inside)
                    self.dots[pointer_id] = dot
                dot.x = x
                dot.y = y
                dot.inside = inside
                dot.released_at = time.monotonic() if released else None
            elif event[0] == "cancel":
                now = time.monotonic()
                for dot in self.dots.values():
                    dot.released_at = now
            elif event[0] == "status":
                _, self.status, self.connected = event
                redraw_static = True

        if redraw_static:
            self._draw_static()
        self._draw_touches()
        self.root.after(8, self._frame)

    def _draw_touches(self) -> None:
        self.canvas.delete("touch")
        width = max(1, self.root.winfo_width())
        height = max(1, self.root.winfo_height())
        now = time.monotonic()
        expired = []

        for pointer_id, dot in self.dots.items():
            progress = 0.0
            if dot.released_at is not None:
                if not self.motion_enabled:
                    expired.append(pointer_id)
                    continue
                progress = (now - dot.released_at) / self.RELEASE_FADE_SECONDS
                if progress >= 1:
                    expired.append(pointer_id)
                    continue

            x = max(0.0, min(1.0, dot.x)) * width
            y = max(0.0, min(1.0, dot.y)) * height
            radius = max(8.0, min(width, height) * 0.10) * (
                1.0 - 0.35 * progress
            )
            color = (
                TOUCH_COLORS[pointer_id % len(TOUCH_COLORS)]
                if dot.inside
                else OUTSIDE
            )
            ring_width = max(1, round(3 * (1.0 - progress)))
            self.canvas.create_oval(
                x - radius,
                y - radius,
                x + radius,
                y + radius,
                outline=color,
                width=ring_width,
                tags="touch",
            )
            inner = radius * 0.35
            self.canvas.create_oval(
                x - inner,
                y - inner,
                x + inner,
                y + inner,
                fill=color,
                outline="",
                tags="touch",
            )

        for pointer_id in expired:
            self.dots.pop(pointer_id, None)

    def _mouse_down(self, event) -> None:
        if not self.edit_mode:
            return
        edge = self._resize_edge_at(event.x, event.y)
        self._drag_mode = f"resize-{edge}" if edge else "move"
        self._drag_start = (
            event.x_root,
            event.y_root,
            int(self.config["x"]),
            int(self.config["y"]),
            int(self.config["w"]),
            int(self.config["h"]),
        )
        self.canvas.grab_set()

    def _mouse_move(self, event) -> None:
        if not self._drag_mode:
            return
        sx, sy, x, y, width, height = self._drag_start
        dx = event.x_root - sx
        dy = event.y_root - sy
        if self._drag_mode == "move":
            self.config["x"] = x + dx
            self.config["y"] = y + dy
        else:
            edge = self._drag_mode.removeprefix("resize-")
            new_x, new_y, new_w, new_h = resize_geometry(
                edge, (x, y, width, height), dx, dy
            )
            self.config.update(x=new_x, y=new_y, w=new_w, h=new_h)
        self._apply_geometry()
        self.root.update_idletasks()
        self._draw_static()

    def _mouse_up(self, _event) -> None:
        self._drag_mode = None
        try:
            self.canvas.grab_release()
        except tk.TclError:
            pass
        self.save_config(self.config)

    def _resize_edge_at(self, x: int, y: int) -> str:
        width = self.root.winfo_width()
        height = self.root.winfo_height()

        near_left = x <= RESIZE_EDGE_PX
        near_right = x >= width - RESIZE_EDGE_PX
        near_top = y <= RESIZE_EDGE_PX
        near_bottom = y >= height - RESIZE_EDGE_PX

        if y <= RESIZE_CORNER_PX or y >= height - RESIZE_CORNER_PX:
            near_left = near_left or x <= RESIZE_CORNER_PX
            near_right = near_right or x >= width - RESIZE_CORNER_PX
        if x <= RESIZE_CORNER_PX or x >= width - RESIZE_CORNER_PX:
            near_top = near_top or y <= RESIZE_CORNER_PX
            near_bottom = near_bottom or y >= height - RESIZE_CORNER_PX

        vertical = "n" if near_top else "s" if near_bottom else ""
        horizontal = "w" if near_left else "e" if near_right else ""
        return vertical + horizontal

    def _mouse_hover(self, event) -> None:
        if not self.edit_mode or self._drag_mode:
            return
        edge = self._resize_edge_at(event.x, event.y)
        cursor = {
            "n": "sb_v_double_arrow",
            "s": "sb_v_double_arrow",
            "e": "sb_h_double_arrow",
            "w": "sb_h_double_arrow",
            "nw": "size_nw_se",
            "se": "size_nw_se",
            "ne": "size_ne_sw",
            "sw": "size_ne_sw",
        }.get(edge, "fleur")
        self.canvas.configure(cursor=cursor)

    def _poll_hotkeys(self) -> None:
        if not self.running:
            return
        user32 = ctypes.windll.user32
        ctrl = bool(user32.GetAsyncKeyState(0x11) & 0x8000)
        shift = bool(user32.GetAsyncKeyState(0x10) & 0x8000)
        o_key = bool(user32.GetAsyncKeyState(ord("O")) & 0x8000)
        q_key = bool(user32.GetAsyncKeyState(ord("Q")) & 0x8000)

        hotkey = ctrl and shift and o_key
        if hotkey and not self._hotkey_down:
            self.set_edit_mode(not self.edit_mode)
        self._hotkey_down = hotkey

        quit_hotkey = ctrl and shift and q_key
        if quit_hotkey and not self._quit_hotkey_down:
            self.close()
        self._quit_hotkey_down = quit_hotkey
        self.root.after(self.HOTKEY_POLL_MS, self._poll_hotkeys)

    def run(self) -> None:
        self.root.mainloop()

    def close(self) -> None:
        if not self.running:
            return
        self.running = False
        self.save_config(self.config)
        if self._edit_button is not None:
            self._edit_button.destroy()
            self._edit_button = None
        self.root.quit()
        self.root.destroy()
