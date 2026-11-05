package io.github.rosinality.artty

import android.content.Context
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.Typeface
import android.text.InputType
import android.util.TypedValue
import android.view.GestureDetector
import android.view.HapticFeedbackConstants
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.ScaleGestureDetector
import android.view.View
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import kotlin.math.ceil
import kotlin.math.floor
import uniffi.artty_android.MouseAction
import uniffi.artty_android.MouseButton
import uniffi.artty_android.Screen

/**
 * A pane's screen, drawn as the server sends it, and the keyboard into it.
 *
 * Text a keyboard composes (a word, a Hangul syllable) goes to the program as it forms:
 * what changed is sent as backspaces and the new characters, so `ㅎ` → `하` → `한` reaches
 * the shell at once rather than when the keyboard commits it.
 */
class TerminalView(context: Context) : View(context) {
    /** The pane shown. */
    var pane: ULong? = null
        set(value) {
            if (field == value) return
            field = value
            sentSize = null
            refresh()
            sendSize()
        }

    /** Ctrl and Alt held from the keys row: for the next key only. */
    var ctrl = false
    var alt = false
    /** Told when Ctrl or Alt is used up, for the keys row. */
    var onModifiers: (() -> Unit)? = null

    private var screen: Screen? = null
    private val paint = Paint(Paint.ANTI_ALIAS_FLAG).apply { typeface = Typeface.MONOSPACE }
    private val density = resources.displayMetrics.density
    private var fontSp = 13f
    private var cellWidth = 0f
    private var cellHeight = 0f
    private var baseline = 0f
    private var sentSize: Pair<Int, Int>? = null

    /** What's being composed, and how much of it the program has been sent. */
    private var composing = ""
    private var composingSent = ""

    init {
        isFocusable = true
        isFocusableInTouchMode = true
        measureFont()
    }

    private fun measureFont() {
        paint.textSize = TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_SP, fontSp, resources.displayMetrics)
        cellWidth = paint.measureText("M")
        val metrics = paint.fontMetrics
        cellHeight = ceil((metrics.descent - metrics.ascent) * 1.12f)
        baseline = -metrics.ascent + (cellHeight - (metrics.descent - metrics.ascent)) / 2
    }

    fun setFontSize(sp: Float) {
        fontSp = sp.coerceIn(7f, 28f)
        measureFont()
        sendSize()
        invalidate()
    }

    /** The pane's screen as last sent. */
    fun refresh() {
        val pane = pane ?: return
        screen = Session.connection?.screen(pane) ?: return
        invalidate()
    }

    /** As many cells as fit: the pane's size while this phone uses it. */
    fun sendSize() {
        val pane = pane ?: return
        if (width == 0 || height == 0 || cellWidth == 0f) return
        val columns = floor(width / cellWidth).toInt().coerceAtLeast(2)
        val lines = floor(height / cellHeight).toInt().coerceAtLeast(1)
        if (sentSize == columns to lines) return
        sentSize = columns to lines
        runCatching {
            Session.connection?.resize(
                pane,
                columns.toUShort(),
                lines.toUShort(),
                cellWidth.toInt().toUShort(),
                cellHeight.toInt().toUShort(),
            )
        }
    }

    override fun onSizeChanged(w: Int, h: Int, oldw: Int, oldh: Int) = sendSize()

    override fun onDraw(canvas: Canvas) {
        val screen = screen
        // Its own rectangle only: drawColor would fill whatever the canvas allows (the
        // Compose host doesn't clip it), the bar above included.
        paint.style = Paint.Style.FILL
        paint.color = argb(screen?.background ?: 0x161a22u)
        canvas.drawRect(0f, 0f, width.toFloat(), height.toFloat(), paint)
        if (screen == null) return
        for ((row, line) in screen.lines.withIndex()) {
            val top = row * cellHeight
            for (rect in line.backgrounds + line.selection) {
                paint.color = argb(rect.color)
                canvas.drawRect(rect.column.toFloat() * cellWidth, top, (rect.column + rect.width).toFloat() * cellWidth, top + cellHeight, paint)
            }
            for (run in line.runs) {
                paint.color = argb(run.fg)
                paint.isFakeBoldText = run.bold
                paint.textSkewX = if (run.italic) -0.2f else 0f
                paint.isStrikeThruText = run.strikeout
                canvas.drawText(run.text, run.column.toFloat() * cellWidth, top + baseline, paint)
            }
            paint.isFakeBoldText = false
            paint.textSkewX = 0f
            paint.isStrikeThruText = false
            for (underline in line.underlines) {
                paint.color = argb(underline.color)
                val y = top + cellHeight - density * 1.5f
                canvas.drawRect(underline.column.toFloat() * cellWidth, y, (underline.column + underline.width).toFloat() * cellWidth, y + density, paint)
            }
        }
        val cursor = screen.cursor ?: return
        val left = cursor.column.toFloat() * cellWidth
        val top = cursor.line.toFloat() * cellHeight
        val wide = if (cursor.wide) 2 else 1
        paint.color = argb(cursor.color)
        if (cursor.block && composing.isEmpty()) {
            canvas.drawRect(left, top, left + wide * cellWidth, top + cellHeight, paint)
            // Its character, in the background's color.
            val line = screen.lines.getOrNull(cursor.line.toInt())
            val run = line?.runs?.firstOrNull { cursor.column >= it.column && cursor.column < it.column + it.cells }
            if (run != null) {
                val offset = (cursor.column - run.column).toInt()
                val text = run.text.codePoints().toArray().getOrNull(offset)?.let { String(Character.toChars(it)) }
                if (text != null) {
                    paint.color = argb(screen.background)
                    canvas.drawText(text, left, top + baseline, paint)
                }
            }
        } else {
            canvas.drawRect(left, top, left + density * 2, top + cellHeight, paint)
        }
    }

    private fun argb(rgb: UInt) = (0xff000000u or rgb).toInt()

    // Touch. A pinch sizes the text. Where the program takes the mouse (htop, vim with
    // `mouse=a`, tmux), a tap clicks, a swipe turns the wheel, and a long press then a drag
    // drags; elsewhere a tap brings the keyboard and a swipe scrolls the history (or, in a
    // full-screen program, sends arrows).

    private val scale = ScaleGestureDetector(context, object : ScaleGestureDetector.SimpleOnScaleGestureListener() {
        override fun onScale(detector: ScaleGestureDetector): Boolean {
            setFontSize(fontSp * detector.scaleFactor)
            return true
        }
    })

    /** The cell under a point, on the screen. */
    private fun cellAt(x: Float, y: Float): Pair<UInt, UInt> {
        val screen = screen
        val lines = (screen?.lines?.size ?: 1).coerceAtLeast(1)
        val columns = (screen?.columns?.toInt() ?: 1).coerceAtLeast(1)
        val line = (y / cellHeight).toInt().coerceIn(0, lines - 1)
        val column = (x / cellWidth).toInt().coerceIn(0, columns - 1)
        return line.toUInt() to column.toUInt()
    }

    private fun wantsMouse() = pane?.let { Session.connection?.wantsMouse(it) } ?: false

    private fun mouse(action: MouseAction, x: Float, y: Float): Boolean {
        val pane = pane ?: return false
        val (line, column) = cellAt(x, y)
        return runCatching { Session.connection?.mouse(pane, MouseButton.LEFT, action, line, column) }.getOrNull() ?: false
    }

    private var scrolled = 0f
    /** A long press began a drag (the program takes the mouse): the cell it's at. */
    private var dragging: Pair<UInt, UInt>? = null

    private val gestures = GestureDetector(context, object : GestureDetector.SimpleOnGestureListener() {
        override fun onDown(e: MotionEvent): Boolean {
            scrolled = 0f
            return true
        }

        override fun onSingleTapUp(e: MotionEvent): Boolean {
            if (wantsMouse()) {
                mouse(MouseAction.PRESS, e.x, e.y)
                mouse(MouseAction.RELEASE, e.x, e.y)
            } else {
                showKeyboard()
            }
            return true
        }

        override fun onLongPress(e: MotionEvent) {
            if (!wantsMouse() || scale.isInProgress) return
            if (mouse(MouseAction.PRESS, e.x, e.y)) {
                dragging = cellAt(e.x, e.y)
                performHapticFeedback(HapticFeedbackConstants.LONG_PRESS)
            }
        }

        override fun onScroll(e1: MotionEvent?, e2: MotionEvent, dx: Float, dy: Float): Boolean {
            if (scale.isInProgress) return true
            scrolled += dy
            val lines = (scrolled / cellHeight).toInt()
            if (lines != 0) {
                scrolled -= lines * cellHeight
                // A finger moving down reads back (the wheel turned up).
                val pane = pane ?: return true
                val (line, column) = cellAt(e2.x, e2.y)
                runCatching { Session.connection?.wheel(pane, -lines, line, column) }
            }
            return true
        }
    })

    override fun onTouchEvent(event: MotionEvent): Boolean {
        // Dragging: the moves and the lift are the program's.
        dragging?.let { at ->
            when (event.actionMasked) {
                MotionEvent.ACTION_MOVE -> {
                    val cell = cellAt(event.x, event.y)
                    if (cell != at) {
                        dragging = cell
                        mouse(MouseAction.MOTION, event.x, event.y)
                    }
                }
                MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                    mouse(MouseAction.RELEASE, event.x, event.y)
                    dragging = null
                }
            }
            return true
        }
        scale.onTouchEvent(event)
        gestures.onTouchEvent(event)
        return true
    }

    fun showKeyboard() {
        requestFocus()
        context.getSystemService(InputMethodManager::class.java).showSoftInput(this, 0)
    }

    // The keyboard.

    override fun onCheckIsTextEditor() = true

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
        outAttrs.inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI or EditorInfo.IME_FLAG_NO_FULLSCREEN or EditorInfo.IME_ACTION_NONE
        return object : BaseInputConnection(this, false) {
            override fun setComposingText(text: CharSequence, newCursorPosition: Int): Boolean {
                compose(text.toString())
                return true
            }

            override fun commitText(text: CharSequence, newCursorPosition: Int): Boolean {
                compose(text.toString())
                composing = ""
                composingSent = ""
                invalidate()
                return true
            }

            override fun finishComposingText(): Boolean {
                composing = ""
                composingSent = ""
                invalidate()
                return true
            }

            override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
                repeat(beforeLength) { key("backspace") }
                return true
            }

            override fun sendKeyEvent(event: KeyEvent): Boolean {
                if (event.action == KeyEvent.ACTION_DOWN) onKeyDown(event.keyCode, event)
                return true
            }
        }
    }

    /** The composing text is now `text`: send what changed since last time. */
    private fun compose(text: String) {
        val common = composingSent.commonPrefixWith(text)
        repeat(composingSent.codePointCount(common.length, composingSent.length)) { key("backspace") }
        type(text.substring(common.length))
        composing = text
        composingSent = text
        invalidate()
    }

    /** Text typed: with Ctrl or Alt held, a single character as a key. */
    fun type(text: String) {
        if (text.isEmpty()) return
        val pane = pane ?: return
        if (text == "\n") return key("enter")
        if ((ctrl || alt) && text.codePointCount(0, text.length) == 1) {
            val c = text[0]
            return key(c.lowercaseChar().toString(), shift = c.isUpperCase())
        }
        runCatching { Session.connection?.text(pane, text) }
    }

    /** A key by name (`enter`, `up`, `c`…), with the keys row's Ctrl and Alt. */
    fun key(name: String, shift: Boolean = false, alt: Boolean = false, ctrl: Boolean = false) {
        val pane = pane ?: return
        val withCtrl = ctrl || this.ctrl
        val withAlt = alt || this.alt
        val taken = runCatching { Session.connection?.key(pane, name, shift, withAlt, withCtrl) }.getOrNull() ?: false
        // A plain character with no meaning as a key (Alt off, say): typed.
        if (!taken && name.length == 1) runCatching { Session.connection?.text(pane, if (shift) name.uppercase() else name) }
        if (this.ctrl || this.alt) {
            this.ctrl = false
            this.alt = false
            onModifiers?.invoke()
        }
    }

    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean {
        val name = when (keyCode) {
            KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER -> "enter"
            KeyEvent.KEYCODE_DEL -> "backspace"
            KeyEvent.KEYCODE_FORWARD_DEL -> "delete"
            KeyEvent.KEYCODE_TAB -> "tab"
            KeyEvent.KEYCODE_ESCAPE -> "escape"
            KeyEvent.KEYCODE_DPAD_UP -> "up"
            KeyEvent.KEYCODE_DPAD_DOWN -> "down"
            KeyEvent.KEYCODE_DPAD_LEFT -> "left"
            KeyEvent.KEYCODE_DPAD_RIGHT -> "right"
            KeyEvent.KEYCODE_MOVE_HOME -> "home"
            KeyEvent.KEYCODE_MOVE_END -> "end"
            KeyEvent.KEYCODE_PAGE_UP -> "pageup"
            KeyEvent.KEYCODE_PAGE_DOWN -> "pagedown"
            KeyEvent.KEYCODE_INSERT -> "insert"
            in KeyEvent.KEYCODE_F1..KeyEvent.KEYCODE_F12 -> "f${keyCode - KeyEvent.KEYCODE_F1 + 1}"
            else -> null
        }
        if (name != null) {
            key(name, event.isShiftPressed, event.isAltPressed, event.isCtrlPressed)
            return true
        }
        // A hardware keyboard's character; Ctrl and Alt with it make a key.
        val unicode = event.getUnicodeChar(event.metaState and (KeyEvent.META_CTRL_MASK or KeyEvent.META_ALT_MASK).inv())
        if (unicode != 0) {
            val text = String(Character.toChars(unicode))
            if (event.isCtrlPressed || event.isAltPressed) key(text.lowercase(), event.isShiftPressed, event.isAltPressed, event.isCtrlPressed)
            else type(text)
            return true
        }
        return super.onKeyDown(keyCode, event)
    }
}
