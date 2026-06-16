package nl.liblisa.sem86

import kotlin.math.hypot

data class TouchDelta(var dx: Float, var dy: Float)

class TrackedPointer {
    var active: Boolean = false
    var pointerId: Int = 0
    var lastX: Float = 0f
    var lastY: Float = 0f
    var distanceMoved: Float = 0f
    var isMoving: Boolean = false
    var downAt: Long = 0
    var upAt: Long = 0
    private var unsentDelta: TouchDelta = TouchDelta(0f, 0f)

    constructor(id: Int, x: Float, y: Float) {
        pointerId = id
        active = true
        downAt = System.currentTimeMillis()
        lastX = x
        lastY = y
    }

    fun updatePosition(x: Float, y: Float) {
        val dx = x - lastX
        val dy = y - lastY

        if (isMoving) {
            distanceMoved += hypot(dx, dy)

            unsentDelta.dx += dx
            unsentDelta.dy += dy

            lastX = x
            lastY = y
        } else if (hypot(dx, dy) > 20.0) {
            isMoving = true;
            lastX = x
            lastY = y
        }
    }

    fun getUnsentDeltas(): TouchDelta {
        val ret = unsentDelta
        unsentDelta = TouchDelta(0f, 0f)
        return ret
    }

    fun distanceTo(other: TrackedPointer): Float {
        return hypot(lastX - other.lastX, lastY - other.lastY)
    }

    fun up() {
        upAt = System.currentTimeMillis()
        active = false
    }
}