package io.github.rosinality.artty

import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Canvas
import android.graphics.DashPathEffect
import android.graphics.Paint
import android.graphics.Path
import android.graphics.RectF
import android.graphics.Typeface
import android.media.AudioManager
import android.text.InputType
import android.util.TypedValue
import android.view.GestureDetector
import android.view.HapticFeedbackConstants
import android.view.InputDevice
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
import kotlin.math.sin
import uniffi.artty_android.LinkSpan
import uniffi.artty_android.MouseAction
import uniffi.artty_android.MouseButton
import uniffi.artty_android.Screen
import uniffi.artty_android.SelectionKind
import uniffi.artty_android.UnderlineStyle

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

    /** Ctrl and Alt held from the keys row: for the next key only, unless locked (a long press). */
    var ctrl = false
    var alt = false
    var locked = false
    /** Told when Ctrl or Alt is used up, for the keys row. */
    var onModifiers: (() -> Unit)? = null

    /** The app's own shortcuts, from a hardware keyboard: whether one was taken. */
    var onShortcut: ((Shortcut) -> Boolean)? = null
    /** A link tapped (asked of the server). */
    var onLink: ((LinkSpan?) -> Unit)? = null
    /** The link shown as tapped, underlined: its line. */
    var link: Pair<Int, LinkSpan>? = null
        set(value) {
            field = value
            invalidate()
        }

    private var screen: Screen? = null
    private val regular = resources.getFont(R.font.jetbrains_mono_regular)
    private val bold = resources.getFont(R.font.jetbrains_mono_bold)
    private val italic = resources.getFont(R.font.jetbrains_mono_italic)
    private val boldItalic = resources.getFont(R.font.jetbrains_mono_bold_italic)
    private val paint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        typeface = regular
        // As the Mac draws it: `->` stays two characters.
        fontFeatureSettings = "'calt' 0, 'liga' 0"
    }
    private val stroke = Paint(Paint.ANTI_ALIAS_FLAG).apply { style = Paint.Style.STROKE }
    private val density = resources.displayMetrics.density
    private val prefs = context.getSharedPreferences("artty", Context.MODE_PRIVATE)
    private var fontSp = prefs.getFloat("font_size", DEFAULT_FONT_SP)
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
        // Focused from a hardware keyboard, Android would gray it all over: the cursor says
        // enough.
        defaultFocusHighlightEnabled = false
        measureFont()
    }

    private fun measureFont() {
        paint.textSize = TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_SP, fontSp, resources.displayMetrics)
        cellWidth = paint.measureText("M")
        val metrics = paint.fontMetrics
        cellHeight = ceil((metrics.descent - metrics.ascent) * 1.12f)
        baseline = -metrics.ascent + (cellHeight - (metrics.descent - metrics.ascent)) / 2
    }

    /** The text's size (kept for next time); `null`: as it starts. */
    fun setFontSize(sp: Float?) {
        fontSp = (sp ?: DEFAULT_FONT_SP).coerceIn(7f, 28f)
        prefs.edit().putFloat("font_size", fontSp).apply()
        measureFont()
        sendSize()
        invalidate()
    }

    /** Shown again after reconnecting: the server is told this phone's size again. */
    fun reattached() {
        sentSize = null
        sendSize()
        refresh()
    }

    /** The pane's screen as last sent. */
    fun refresh() {
        val pane = pane ?: return
        screen = Session.connection?.screen(pane) ?: return
        // Changed: the cursor shows (and blinks again from there).
        cursorOn = true
        removeCallbacks(blink)
        if (screen?.cursor?.blinking == true) postDelayed(blink, BLINK_MS)
        val shown = screen?.lines?.flatMap { line -> line.images.map { it.image } }?.toSet() ?: emptySet()
        bitmaps.keys.retainAll(shown)
        invalidate()
    }

    // The cursor blinks where the program asks, while this has the keyboard.
    private var cursorOn = true
    private val blink = object : Runnable {
        override fun run() {
            cursorOn = !cursorOn || !hasFocus()
            invalidate()
            // Out of sight, it waits for the next screen.
            if (screen?.cursor?.blinking == true && windowVisibility == VISIBLE) postDelayed(this, BLINK_MS)
        }
    }

    override fun onDetachedFromWindow() {
        super.onDetachedFromWindow()
        removeCallbacks(blink)
    }

    // Images, decoded once each (off the main thread), while a screen shows them.
    private class Shown(val bitmap: Bitmap, val columns: Float, val rows: Float)
    private val bitmaps = HashMap<ULong, Shown?>()

    /** An image arrived: drawn, if it's waited for. */
    fun imageArrived(id: ULong) {
        if (bitmaps.containsKey(id) && bitmaps[id] == null) {
            bitmaps.remove(id)
            invalidate()
        }
    }

    private fun bitmap(id: ULong): Shown? {
        if (bitmaps.containsKey(id)) return bitmaps[id]
        bitmaps[id] = null
        val connection = Session.connection ?: return null
        Thread {
            val image = runCatching { connection.image(id) }.getOrNull() ?: return@Thread
            val bitmap = runCatching {
                if (image.encoded) {
                    BitmapFactory.decodeByteArray(image.data, 0, image.data.size)
                } else {
                    Bitmap.createBitmap(image.width.toInt(), image.height.toInt(), Bitmap.Config.ARGB_8888).apply {
                        copyPixelsFromBuffer(java.nio.ByteBuffer.wrap(image.data))
                    }
                }
            }.getOrNull() ?: return@Thread
            post {
                if (bitmaps.containsKey(id)) {
                    bitmaps[id] = Shown(bitmap, image.columns, image.rows)
                    invalidate()
                }
            }
        }.start()
        return null
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
            for (rect in line.backgrounds + line.matches + line.selection) {
                paint.color = argb(rect.color)
                canvas.drawRect(rect.column.toFloat() * cellWidth, top, (rect.column + rect.width).toFloat() * cellWidth, top + cellHeight, paint)
            }
            // Each run of image cells shows its part: the whole image placed where its top
            // left cell would be, clipped to these cells.
            for (span in line.images) {
                val shown = bitmap(span.image) ?: continue
                val left = (span.column.toFloat() - span.tileColumn.toFloat()) * cellWidth
                val imageTop = top - span.tileRow.toFloat() * cellHeight
                canvas.save()
                canvas.clipRect(span.column.toFloat() * cellWidth, top, (span.column + span.cells).toFloat() * cellWidth, top + cellHeight)
                canvas.drawBitmap(shown.bitmap, null, RectF(left, imageTop, left + shown.columns * cellWidth, imageTop + shown.rows * cellHeight), paint)
                canvas.restore()
            }
            for (run in line.runs) {
                paint.color = argb(run.fg)
                paint.typeface = when {
                    run.bold && run.italic -> boldItalic
                    run.bold -> bold
                    run.italic -> italic
                    else -> regular
                }
                paint.isStrikeThruText = run.strikeout
                canvas.drawText(run.text, run.column.toFloat() * cellWidth, top + baseline, paint)
            }
            paint.typeface = regular
            paint.isStrikeThruText = false
            for (underline in line.underlines) {
                underline(canvas, top, underline.column.toInt(), underline.width.toInt(), underline.color, underline.style)
            }
            link?.let { (at, span) ->
                if (at == row) underline(canvas, top, span.start.toInt(), (span.end - span.start).toInt(), screen.foreground, UnderlineStyle.SINGLE)
            }
        }
        val cursor = screen.cursor ?: return
        if (!cursorOn) return
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

    private fun underline(canvas: Canvas, top: Float, column: Int, width: Int, color: UInt, style: UnderlineStyle) {
        val left = column * cellWidth
        val right = (column + width) * cellWidth
        val y = top + cellHeight - density * 1.5f
        stroke.color = argb(color)
        stroke.strokeWidth = density
        stroke.pathEffect = null
        when (style) {
            UnderlineStyle.SINGLE -> canvas.drawLine(left, y, right, y, stroke)
            UnderlineStyle.DOUBLE -> {
                canvas.drawLine(left, y, right, y, stroke)
                canvas.drawLine(left, y - density * 2, right, y - density * 2, stroke)
            }
            UnderlineStyle.DOTTED -> {
                stroke.pathEffect = DashPathEffect(floatArrayOf(density, density * 2), 0f)
                canvas.drawLine(left, y, right, y, stroke)
            }
            UnderlineStyle.DASHED -> {
                stroke.pathEffect = DashPathEffect(floatArrayOf(density * 4, density * 2), 0f)
                canvas.drawLine(left, y, right, y, stroke)
            }
            UnderlineStyle.CURLY -> {
                // A wave a cell wide.
                val path = Path()
                val amplitude = density
                var x = left
                path.moveTo(x, y)
                while (x < right) {
                    x = (x + density).coerceAtMost(right)
                    path.lineTo(x, y + amplitude * sin((x - left) / cellWidth * 2 * Math.PI).toFloat())
                }
                canvas.drawPath(path, stroke)
            }
        }
    }

    // Touch. A pinch sizes the text; two fingers swiped sideways go to the next tab (to the
    // left) or the one before. Where the program takes the mouse (htop, vim with
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

    /** Touches select even where the program takes the mouse (the keys row's Select). */
    var selectMode = false
    /** A long press began selecting, and the finger is still down. */
    private var selecting = false
    /** Something is selected; told when that changes (for the Copy bar). */
    var hasSelection = false
        private set
    var onSelection: ((Boolean) -> Unit)? = null

    private fun selects() = selectMode || !wantsMouse()

    /** In the right half of its cell (a selection ends after it). */
    private fun rightHalf(x: Float) = (x / cellWidth) % 1f >= 0.5f

    /** Where the selection began (a long press): for taking the whole line. */
    private var anchor: Pair<UInt, UInt>? = null

    /** The selection grown to the whole line it began on. */
    fun selectLine() {
        val pane = pane ?: return
        val (line, column) = anchor ?: return
        runCatching { Session.connection?.selectStart(pane, SelectionKind.LINES, line, column, false) }
    }

    fun clearSelection() {
        if (!hasSelection) return
        hasSelection = false
        pane?.let { runCatching { Session.connection?.selectClear(it) } }
        onSelection?.invoke(false)
    }

    /** The selected text (asked of the server, off the main thread), then the selection goes. */
    fun copy(done: (String?) -> Unit) {
        val pane = pane ?: return done(null)
        val connection = Session.connection ?: return done(null)
        Thread {
            val text = runCatching { connection.selectionText(pane) }.getOrNull()
            post {
                done(text)
                clearSelection()
            }
        }.start()
    }

    /** Whether pasting `text` here could run several commands (worth asking first). */
    fun pasteRunsLines(text: String): Boolean {
        val pane = pane ?: return false
        return text.trimEnd().contains('\n') && Session.connection?.pastesWhole(pane) == false
    }

    fun paste(text: String) {
        val pane = pane ?: return
        runCatching { Session.connection?.paste(pane, text) }
    }

    private val gestures = GestureDetector(context, object : GestureDetector.SimpleOnGestureListener() {
        override fun onDown(e: MotionEvent): Boolean {
            scrolled = 0f
            return true
        }

        override fun onSingleTapUp(e: MotionEvent): Boolean {
            // A tap puts a selection away.
            if (hasSelection) {
                clearSelection()
                return true
            }
            if (link != null) {
                link = null
                onLink?.invoke(null)
            }
            if (!selectMode && wantsMouse()) {
                mouse(MouseAction.PRESS, e.x, e.y)
                mouse(MouseAction.RELEASE, e.x, e.y)
            } else {
                tapped(e.x, e.y)
            }
            return true
        }

        override fun onLongPress(e: MotionEvent) {
            if (scale.isInProgress) return
            // Selecting: the word pressed, then as far as the finger goes.
            if (selects()) {
                val pane = pane ?: return
                val (line, column) = cellAt(e.x, e.y)
                anchor = line to column
                runCatching { Session.connection?.selectStart(pane, SelectionKind.WORDS, line, column, rightHalf(e.x)) }
                selecting = true
                hasSelection = true
                onSelection?.invoke(true)
                performHapticFeedback(HapticFeedbackConstants.LONG_PRESS)
                return
            }
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

    // Two fingers: where they began, and how far apart (a pinch changes that; a swipe doesn't).
    private var twoFrom: Pair<Float, Float>? = null
    private var twoSpan = 0f
    /** The two fingers swiped: the rest of their touch is done with. */
    private var swiped = false

    private fun centre(event: MotionEvent): Pair<Float, Float> =
        ((event.getX(0) + event.getX(1)) / 2) to ((event.getY(0) + event.getY(1)) / 2)

    private fun span(event: MotionEvent) = kotlin.math.hypot(event.getX(0) - event.getX(1), event.getY(0) - event.getY(1))

    /** A two-finger swipe sideways: another tab. Whether the touch is taken by it. */
    private fun swipe(event: MotionEvent): Boolean {
        when (event.actionMasked) {
            MotionEvent.ACTION_POINTER_DOWN -> if (event.pointerCount == 2) {
                twoFrom = centre(event)
                twoSpan = span(event)
                swiped = false
            }
            MotionEvent.ACTION_MOVE -> {
                val from = twoFrom
                if (event.pointerCount == 2 && from != null && !swiped) {
                    val (x, y) = centre(event)
                    val (dx, dy) = (x - from.first) to (y - from.second)
                    val pinched = kotlin.math.abs(span(event) - twoSpan)
                    val far = SWIPE_DP * resources.displayMetrics.density
                    if (kotlin.math.abs(dx) > far && kotlin.math.abs(dx) > 2 * kotlin.math.abs(dy) && pinched < kotlin.math.abs(dx) / 2) {
                        swiped = true
                        performHapticFeedback(HapticFeedbackConstants.CLOCK_TICK)
                        onShortcut?.invoke(if (dx < 0) Shortcut.NextTab else Shortcut.PreviousTab)
                    }
                }
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                val done = swiped
                twoFrom = null
                swiped = false
                return done
            }
        }
        return swiped
    }

    override fun onTouchEvent(event: MotionEvent): Boolean {
        if (swipe(event)) return true
        // Selecting: the moves take it along.
        if (selecting) {
            when (event.actionMasked) {
                MotionEvent.ACTION_MOVE -> {
                    val pane = pane
                    val (line, column) = cellAt(event.x, event.y)
                    if (pane != null) runCatching { Session.connection?.selectUpdate(pane, line, column, rightHalf(event.x)) }
                }
                MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> selecting = false
            }
            return true
        }
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

    /**
     * A tap on the text: a link there is offered (the server knows: OSC 8 links, URLs in the
     * text); else, or if it's slow to say, the keyboard comes.
     */
    private fun tapped(x: Float, y: Float) {
        val pane = pane ?: return showKeyboard()
        val connection = Session.connection ?: return showKeyboard()
        val (line, column) = cellAt(x, y)
        var answered = false
        val keyboard = Runnable {
            answered = true
            showKeyboard()
        }
        postDelayed(keyboard, LINK_WAIT_MS)
        Thread {
            val span = runCatching { connection.linkAt(pane, line, column) }.getOrNull()
            post {
                if (span == null) {
                    if (!answered) {
                        removeCallbacks(keyboard)
                        showKeyboard()
                    }
                    return@post
                }
                removeCallbacks(keyboard)
                link = line.toInt() to span
                onLink?.invoke(span)
            }
        }.start()
    }

    // A mouse's wheel, or two fingers on a touchpad.
    override fun onGenericMotionEvent(event: MotionEvent): Boolean {
        if (event.isFromSource(InputDevice.SOURCE_CLASS_POINTER) && event.actionMasked == MotionEvent.ACTION_SCROLL) {
            val pane = pane ?: return true
            wheelTurned += event.getAxisValue(MotionEvent.AXIS_VSCROLL) * WHEEL_LINES
            val lines = wheelTurned.toInt()
            if (lines != 0) {
                wheelTurned -= lines
                val (line, column) = cellAt(event.x, event.y)
                runCatching { Session.connection?.wheel(pane, lines, line, column) }
            }
            return true
        }
        return super.onGenericMotionEvent(event)
    }

    private var wheelTurned = 0f

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
        if (!type(text.substring(common.length))) {
            // It went as a key (Ctrl held, say): what the keyboard composes starts afresh.
            composing = ""
            composingSent = ""
            context.getSystemService(InputMethodManager::class.java).restartInput(this)
            return
        }
        composing = text
        composingSent = text
        invalidate()
    }

    /**
     * Text typed: with Ctrl or Alt held (the keys row, a volume key), a single character as a
     * key. Returns whether it went as text.
     */
    fun type(text: String): Boolean {
        if (text.isEmpty()) return true
        val pane = pane ?: return true
        if (text == "\n") {
            key("enter")
            return false
        }
        if (text.codePointCount(0, text.length) == 1) {
            val c = text[0]
            if (volumeUp != null && volumeKey(c)) return false
            if (ctrl || alt || volumeDown != null) {
                key(c.lowercaseChar().toString(), shift = c.isUpperCase())
                return false
            }
        }
        runCatching { Session.connection?.text(pane, text) }
        return true
    }

    /** A key by name (`enter`, `up`, `c`…), with the keys row's Ctrl and Alt (and volume down's Ctrl). */
    fun key(name: String, shift: Boolean = false, alt: Boolean = false, ctrl: Boolean = false) {
        val pane = pane ?: return
        val withCtrl = ctrl || this.ctrl || volumeDown != null
        val withAlt = alt || this.alt
        volumeDown?.let { volumeDown = true }
        val taken = runCatching { Session.connection?.key(pane, name, shift, withAlt, withCtrl) }.getOrNull() ?: false
        // A plain character with no meaning as a key (Alt off, say): typed.
        if (!taken && name.length == 1) runCatching { Session.connection?.text(pane, if (shift) name.uppercase() else name) }
        if ((this.ctrl || this.alt) && !locked) {
            this.ctrl = false
            this.alt = false
            onModifiers?.invoke()
        }
    }

    // The volume keys, held: down is Ctrl, up makes the keys a phone keyboard lacks (as
    // Termux has them: W A S D arrows, E Esc, T Tab, 1–0 F1–F10, P N pages up and down, H
    // Home, L End). Pressed alone, they turn the volume as ever. Held: whether used since.
    private var volumeDown: Boolean? = null
    private var volumeUp: Boolean? = null

    private fun volumeKey(c: Char): Boolean {
        val name = when (c.lowercaseChar()) {
            'w' -> "up"
            'a' -> "left"
            's' -> "down"
            'd' -> "right"
            'e' -> "escape"
            't' -> "tab"
            'p' -> "pageup"
            'n' -> "pagedown"
            'h' -> "home"
            'l' -> "end"
            in '1'..'9' -> "f${c - '0'}"
            '0' -> "f10"
            else -> return false
        }
        volumeUp = true
        key(name)
        return true
    }

    /** The volume keys as keys (Settings); else they only turn the volume, as ever. */
    private val volumeKeys get() = prefs.getBoolean("volume_keys", true)

    override fun onKeyUp(keyCode: Int, event: KeyEvent): Boolean {
        if (!volumeKeys && keyCode in VOLUME_KEYS) return super.onKeyUp(keyCode, event)
        val direction = when (keyCode) {
            KeyEvent.KEYCODE_VOLUME_DOWN -> volumeDown.also { volumeDown = null }?.let { used -> if (used) 0 else AudioManager.ADJUST_LOWER }
            KeyEvent.KEYCODE_VOLUME_UP -> volumeUp.also { volumeUp = null }?.let { used -> if (used) 0 else AudioManager.ADJUST_RAISE }
            else -> return super.onKeyUp(keyCode, event)
        }
        if (direction != null && direction != 0) {
            context.getSystemService(AudioManager::class.java)
                .adjustSuggestedStreamVolume(direction, AudioManager.USE_DEFAULT_STREAM_TYPE, AudioManager.FLAG_SHOW_UI)
        }
        return true
    }

    /**
     * A hardware keyboard's shortcut for the app, if it is one: Ctrl+Shift with a key, as Linux
     * terminals have them (or Meta, where the system lets it through: Android keeps most Meta
     * keys for itself).
     */
    private fun shortcut(keyCode: Int, event: KeyEvent): Shortcut? {
        val ctrlShift = event.isCtrlPressed && event.isShiftPressed
        if (keyCode == KeyEvent.KEYCODE_TAB && event.isCtrlPressed) return if (event.isShiftPressed) Shortcut.PreviousTab else Shortcut.NextTab
        if (!event.isMetaPressed && !ctrlShift) return null
        return when (keyCode) {
            KeyEvent.KEYCODE_T -> Shortcut.NewTab
            KeyEvent.KEYCODE_W -> Shortcut.CloseTab
            KeyEvent.KEYCODE_C -> Shortcut.Copy
            KeyEvent.KEYCODE_V -> Shortcut.Paste
            KeyEvent.KEYCODE_F -> Shortcut.Find
            KeyEvent.KEYCODE_LEFT_BRACKET, KeyEvent.KEYCODE_PAGE_UP -> Shortcut.PreviousTab
            KeyEvent.KEYCODE_RIGHT_BRACKET, KeyEvent.KEYCODE_PAGE_DOWN -> Shortcut.NextTab
            KeyEvent.KEYCODE_EQUALS, KeyEvent.KEYCODE_PLUS -> Shortcut.Bigger
            KeyEvent.KEYCODE_MINUS -> Shortcut.Smaller
            KeyEvent.KEYCODE_0 -> Shortcut.ResetSize
            in KeyEvent.KEYCODE_1..KeyEvent.KEYCODE_9 -> Shortcut.Tab(keyCode - KeyEvent.KEYCODE_1)
            else -> null
        }
    }

    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean {
        if (!volumeKeys && keyCode in VOLUME_KEYS) return super.onKeyDown(keyCode, event)
        when (keyCode) {
            KeyEvent.KEYCODE_VOLUME_DOWN -> {
                if (volumeDown == null) volumeDown = false
                return true
            }
            KeyEvent.KEYCODE_VOLUME_UP -> {
                if (volumeUp == null) volumeUp = false
                return true
            }
        }
        shortcut(keyCode, event)?.let { if (onShortcut?.invoke(it) == true) return true }
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
            if (volumeUp != null && volumeKey(text[0])) return true
            if (event.isCtrlPressed || event.isAltPressed) key(text.lowercase(), event.isShiftPressed, event.isAltPressed, event.isCtrlPressed)
            else type(text)
            return true
        }
        return super.onKeyDown(keyCode, event)
    }

    fun biggerText(by: Float) = setFontSize(fontSp + by)

    private companion object {
        const val DEFAULT_FONT_SP = 13f
        const val BLINK_MS = 530L
        /** How long a tap waits to hear of a link before bringing the keyboard. */
        const val LINK_WAIT_MS = 250L
        const val WHEEL_LINES = 3f
        /** How far two fingers go sideways to be a swipe to another tab. */
        const val SWIPE_DP = 72f
        val VOLUME_KEYS = setOf(KeyEvent.KEYCODE_VOLUME_DOWN, KeyEvent.KEYCODE_VOLUME_UP)
    }
}

/** What the app's hardware-keyboard shortcuts do. */
sealed interface Shortcut {
    data object NewTab : Shortcut
    data object CloseTab : Shortcut
    data object NextTab : Shortcut
    data object PreviousTab : Shortcut
    data class Tab(val index: Int) : Shortcut
    data object Copy : Shortcut
    data object Paste : Shortcut
    data object Find : Shortcut
    data object Bigger : Shortcut
    data object Smaller : Shortcut
    data object ResetSize : Shortcut
}
