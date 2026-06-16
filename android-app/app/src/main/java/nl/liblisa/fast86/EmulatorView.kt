package nl.liblisa.sem86

import android.content.Context
import android.graphics.SurfaceTexture
import android.os.Build
import android.os.VibrationEffect
import android.os.Vibrator
import android.os.VibratorManager
import android.text.InputType
import android.view.Choreographer
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.Surface
import android.view.TextureView
import android.view.View
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import android.view.inputmethod.TextAttribute

data class KeyMapping(val keyCode: Int, val shift: Boolean = false)

class EmulatorView(context: Context, val emulator: Emulator): TextureView(context), TextureView.SurfaceTextureListener, View.OnKeyListener {
    private val pointerTracker: PointerTracker = PointerTracker(
        postDelayed = { runnable, delay -> this.postDelayed(runnable, delay) },
        emulator = emulator,
        showKeyboard = {
            requestFocus()

            val imm = context.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
            imm.showSoftInput(this, InputMethodManager.SHOW_IMPLICIT)
        },
        vibrate = {
            val vibrator = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                val vibratorManager =
                    context.getSystemService(Context.VIBRATOR_MANAGER_SERVICE) as VibratorManager
                vibratorManager.defaultVibrator
            } else {
                @Suppress("DEPRECATION")
                context.getSystemService(Context.VIBRATOR_SERVICE) as Vibrator
            }

            vibrator.vibrate(VibrationEffect.createOneShot(25, VibrationEffect.DEFAULT_AMPLITUDE))
        }
    )
    private var storedSurface: Surface? = null

    init {
        isFocusable = true
        isFocusableInTouchMode = true
        surfaceTextureListener = this

        setOnKeyListener(this)
    }

    private val frameCallback = object : Choreographer.FrameCallback {
        override fun doFrame(frameTimeNanos: Long) {
            if (!canRender) return

            emulator.render()

            // request next frame
            Choreographer.getInstance().postFrameCallback(this)
        }
    }

    private var canRender = false

    override fun onSurfaceTextureAvailable(surfaceTexture: SurfaceTexture, width: Int, height: Int) {
        println("Surface available, connecting it now.")
        val surface = Surface(surfaceTexture)
        RustInterop.connectSurface(emulator.runningEmulator!!.emulatorIndex, surface)
        RustInterop.resizeSurface(emulator.runningEmulator!!.emulatorIndex, width, height)
        canRender = true
        Choreographer.getInstance().postFrameCallback(frameCallback)

        storedSurface = surface
    }

    override fun onSurfaceTextureSizeChanged(surface: SurfaceTexture, width: Int, height: Int) {
        println("Surface changed: width=$width, height=$height")
        RustInterop.resizeSurface(emulator.runningEmulator!!.emulatorIndex, width, height)
    }

    override fun onSurfaceTextureDestroyed(surface: SurfaceTexture): Boolean {
        println("Surface destroyed")
        canRender = false
        emulator.dropSurface()
        return true
    }

    override fun onSurfaceTextureUpdated(surface: SurfaceTexture) {}

    override fun onCreateInputConnection(outAttrs: EditorInfo?): InputConnection? {
        outAttrs?.actionLabel = null
        outAttrs?.inputType =
            InputType.TYPE_CLASS_TEXT or
                    InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS or
                    InputType.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD

        outAttrs?.imeOptions =
            EditorInfo.IME_FLAG_NO_FULLSCREEN or
                    EditorInfo.IME_FLAG_NO_EXTRACT_UI
        return object : BaseInputConnection(this, true) {
            override fun commitText(text: CharSequence?, newCursorPosition: Int): Boolean {
                println("Commit text: '${text}'")

                text?.forEach {
                    emitChar(it)
                }

                return super.commitText(text, newCursorPosition)
            }

            override fun setComposingText(
                text: CharSequence,
                newCursorPosition: Int,
                textAttribute: TextAttribute?
            ): Boolean {
                finishComposingText()
                return true
            }

            fun charToKeyCode(c: Char): KeyMapping? {
                return when (c) {
                    // Letters
                    in 'a'..'z' -> KeyMapping(KeyEvent.KEYCODE_A + (c - 'a'))
                    in 'A'..'Z' -> KeyMapping(KeyEvent.KEYCODE_A + (c - 'A'), shift = true)

                    // Numbers
                    in '0'..'9' -> KeyMapping(KeyEvent.KEYCODE_0 + (c - '0'))

                    // Space, Enter, Tab
                    ' '  -> KeyMapping(KeyEvent.KEYCODE_SPACE)
                    '\n' -> KeyMapping(KeyEvent.KEYCODE_ENTER)
                    '\t' -> KeyMapping(KeyEvent.KEYCODE_TAB)

                    // Shift symbols (US QWERTY)
                    '!'  -> KeyMapping(KeyEvent.KEYCODE_1, shift = true)
                    '@'  -> KeyMapping(KeyEvent.KEYCODE_2, shift = true)
                    '#'  -> KeyMapping(KeyEvent.KEYCODE_3, shift = true)
                    '$'  -> KeyMapping(KeyEvent.KEYCODE_4, shift = true)
                    '%'  -> KeyMapping(KeyEvent.KEYCODE_5, shift = true)
                    '^'  -> KeyMapping(KeyEvent.KEYCODE_6, shift = true)
                    '&'  -> KeyMapping(KeyEvent.KEYCODE_7, shift = true)
                    '*'  -> KeyMapping(KeyEvent.KEYCODE_8, shift = true)
                    '('  -> KeyMapping(KeyEvent.KEYCODE_9, shift = true)
                    ')'  -> KeyMapping(KeyEvent.KEYCODE_0, shift = true)

                    '_'  -> KeyMapping(KeyEvent.KEYCODE_MINUS, shift = true)
                    '+'  -> KeyMapping(KeyEvent.KEYCODE_EQUALS, shift = true)
                    '{'  -> KeyMapping(KeyEvent.KEYCODE_LEFT_BRACKET, shift = true)
                    '}'  -> KeyMapping(KeyEvent.KEYCODE_RIGHT_BRACKET, shift = true)
                    '|'  -> KeyMapping(KeyEvent.KEYCODE_BACKSLASH, shift = true)
                    ':'  -> KeyMapping(KeyEvent.KEYCODE_SEMICOLON, shift = true)
                    '"'  -> KeyMapping(KeyEvent.KEYCODE_APOSTROPHE, shift = true)
                    '<'  -> KeyMapping(KeyEvent.KEYCODE_COMMA, shift = true)
                    '>'  -> KeyMapping(KeyEvent.KEYCODE_PERIOD, shift = true)
                    '?'  -> KeyMapping(KeyEvent.KEYCODE_SLASH, shift = true)
                    '~'  -> KeyMapping(KeyEvent.KEYCODE_GRAVE, shift = true)

                    // Non-shift symbols
                    '-'  -> KeyMapping(KeyEvent.KEYCODE_MINUS)
                    '='  -> KeyMapping(KeyEvent.KEYCODE_EQUALS)
                    '['  -> KeyMapping(KeyEvent.KEYCODE_LEFT_BRACKET)
                    ']'  -> KeyMapping(KeyEvent.KEYCODE_RIGHT_BRACKET)
                    '\\' -> KeyMapping(KeyEvent.KEYCODE_BACKSLASH)
                    ';'  -> KeyMapping(KeyEvent.KEYCODE_SEMICOLON)
                    '\'' -> KeyMapping(KeyEvent.KEYCODE_APOSTROPHE)
                    ','  -> KeyMapping(KeyEvent.KEYCODE_COMMA)
                    '.'  -> KeyMapping(KeyEvent.KEYCODE_PERIOD)
                    '/'  -> KeyMapping(KeyEvent.KEYCODE_SLASH)
                    '`'  -> KeyMapping(KeyEvent.KEYCODE_GRAVE)

                    else -> null
                }
            }

            fun emitChar(c: Char) {
                val k = charToKeyCode(c.lowercaseChar()) ?: return

                if (k.shift) RustInterop.keyboardInput(emulator.runningEmulator!!.emulatorIndex, true, KeyEvent.KEYCODE_SHIFT_LEFT)
                RustInterop.keyboardInput(emulator.runningEmulator!!.emulatorIndex, true, k.keyCode)
                RustInterop.keyboardInput(emulator.runningEmulator!!.emulatorIndex, false, k.keyCode)
                if (k.shift) RustInterop.keyboardInput(emulator.runningEmulator!!.emulatorIndex, false, KeyEvent.KEYCODE_SHIFT_LEFT)
            }

            override fun sendKeyEvent(event: KeyEvent): Boolean {
                println("Key event: $event")
                val isDown = event.action == KeyEvent.ACTION_DOWN
                RustInterop.keyboardInput(emulator.runningEmulator!!.emulatorIndex, isDown, event.keyCode)
                return super.sendKeyEvent(event)
            }
        }
    }

    override fun onKey(p0: View?, p1: Int, p2: KeyEvent?): Boolean {
        println("Key: ${p1} ${p2}")
        return false
    }

    override fun onKeyDown(keyCode: Int, event: KeyEvent?): Boolean {
        println("Key down: ${keyCode} - ${event}")
        return false
    }

    override fun onKeyUp(keyCode: Int, event: KeyEvent?): Boolean {
        println("Key up: ${keyCode} - ${event}")
        return false
    }

    override fun onTouchEvent(event: MotionEvent?): Boolean {
        if (event == null) return super.onTouchEvent(event)

        pointerTracker.handleEvent(event)
        return true
    }
}