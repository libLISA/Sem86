package nl.liblisa.sem86

import android.view.MotionEvent
import android.view.inputmethod.InputMethodManager
import androidx.compose.ui.platform.LocalHapticFeedback
import kotlin.math.abs
import kotlin.math.hypot
import kotlin.math.truncate

class PointerTracker(private val postDelayed: (Runnable, Long) -> Boolean, private val emulator: Emulator, private val showKeyboard: () -> Unit, private val vibrate: () -> Unit) {
    var lastPointers: List<TrackedPointer> = listOf()
    var currentPointers: MutableList<TrackedPointer> = mutableListOf()

    private var draggingFromDoubleTap = false

    private val doubleTapTimeout = 200L // ms
    private var pendingZDelta = 0.0

    fun isDoubleTap(): Boolean {
        val lastTouchEnded = lastPointers.maxOfOrNull { p -> p.upAt } ?: 0
        val lastTouchDown = currentPointers.maxOfOrNull { p -> p.downAt } ?: 0
        val delay = (lastTouchDown - lastTouchEnded)
        println("Touch down ${delay}ms after last gesture (which used ${lastPointers.size} pointers, currently have ${currentPointers.size})")

        if (delay > doubleTapTimeout || lastPointers.size != currentPointers.size) {
            return false
        }

        // Pointers shouldn't move for taps
        if (lastPointers.any { p -> p.isMoving } || currentPointers.any { p -> p.isMoving }) {
            println("Some pointers moved, so no double-top-to-drag")
            return false
        }

        // Greedily check if we can pair all currentPointers to lastPointers.
        // Technically this could detect scenarios where one pointer is within 50 distance of two lastPointers.
        // This is unlikely in practice because fingers are big.
        return lastPointers.all { l -> currentPointers.any { p -> p.distanceTo(l) < 50 } } && currentPointers.all { p -> lastPointers.any { l -> p.distanceTo(l) < 50 }}
    }

    fun anyPointerMoved(): Boolean {
        return currentPointers.any { p -> p.isMoving }
    }

    fun handleEvent(event: MotionEvent) {
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> {
                val newPointerIndex = event.actionIndex
                val newId = event.getPointerId(newPointerIndex)

                val x = event.getX(newPointerIndex)
                val y = event.getY(newPointerIndex)
                println("Pointer down (id=${newId}, x={x}, y={y})")

                currentPointers.add(TrackedPointer(newId, x, y))

                if (isDoubleTap()) {
                    println("Starting ${currentPointers.size}-pointer drag")
                    draggingFromDoubleTap = true
                }

                if (draggingFromDoubleTap && currentPointers.size > 2) {
                    draggingFromDoubleTap = false
                }
            }

            MotionEvent.ACTION_POINTER_UP -> {
                val pointerId = event.getPointerId(event.actionIndex)
                val pointer = currentPointers.find { p -> p.pointerId == pointerId } !!
                pointer.up()
            }

            MotionEvent.ACTION_MOVE -> {
                // Move all tracked pointers
                for (index in 0..<event.pointerCount) {
                    val id = event.getPointerId(index)
                    val x = event.getX(index)
                    val y = event.getY(index)

                    val pointer = currentPointers.find { p -> p.pointerId == id } !!;
                    pointer.updatePosition(x, y)
                }

                // Check if we should move the mouse cursor
                if ((draggingFromDoubleTap || currentPointers.size == 1) && currentPointers[0].isMoving) {
                    val delta = currentPointers[0].getUnsentDeltas()
                    emulator.mouseMove(delta.dx.toDouble(), delta.dy.toDouble())
                }

                if (currentPointers.size == 2 && currentPointers[0].isMoving) {
                    val delta = currentPointers[0].getUnsentDeltas();
                    println("Mouse scroll delta: ${delta.dy}");
                    pendingZDelta += delta.dy / 300.0;

                    if (abs(pendingZDelta) > 1.0) {
                        emulator.mouseScroll(truncate(pendingZDelta))
                        pendingZDelta -= truncate(pendingZDelta)
                        vibrate()
                    }
                }
            }

            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                val pointerId = event.getPointerId(event.actionIndex)
                val pointer = currentPointers.find { p -> p.pointerId == pointerId } !!
                pointer.up()

                if (!anyPointerMoved()) {
                    if (draggingFromDoubleTap) {
                        if (currentPointers.size <= 2) {
                            println("Double tap without moving, sending extra mouse clicks")
                            // Dragging for a double tap, but we didn't drag.
                            // Quickly release the mouse and send a second tap.
                            emulator.mouseButtonState(false, false)

                            val lmb = currentPointers.size == 1
                            val rmb = currentPointers.size == 2
                            this.postDelayed({
                                println("Mouse down (lmb=${lmb}, rmb=${rmb})")
                                emulator.mouseButtonState(lmb, rmb)
                            }, 25)

                            this.postDelayed({
                                println("All mouse buttons up")
                                emulator.mouseButtonState(false, false)
                            }, 50)
                        }
                    } else {
                        if (currentPointers.size == 1) {
                            // One-finger tap without moving

                            println("LMB down")
                            emulator.mouseButtonState(true, false)
                            this.postDelayed({
                                if (!draggingFromDoubleTap) {
                                    println("LMB up")
                                    emulator.mouseButtonState(false, false)
                                }
                            }, doubleTapTimeout)
                        } else if (currentPointers.size == 2) {
                            // Two-finger tap without moving

                            println("RMB down")
                            emulator.mouseButtonState(false, true)
                            this.postDelayed({
                                if (!draggingFromDoubleTap) {
                                    println("RMB up")
                                    emulator.mouseButtonState(false, false)
                                }
                            }, doubleTapTimeout)
                        } else if (currentPointers.size == 3) {
                            showKeyboard()
                        }
                    }
                } else {
                    if (draggingFromDoubleTap) {
                        println("All mouse buttons up: End of drag")
                        emulator.mouseButtonState(false, false)
                    }
                }

                draggingFromDoubleTap = false;

                pendingZDelta = 0.0
                lastPointers = currentPointers
                currentPointers = mutableListOf()
            }
        }
    }
}