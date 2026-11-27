package dev.coqo.sample

import kotlin.math.PI
import kotlin.math.sqrt

const val LIMIT = 10

val greeting: String = "hi"

interface Shape {
    fun area(): Double
}

data class Circle(val radius: Double) : Shape {
    val diameter: Double
        get() = radius * 2

    override fun area(): Double {
        return PI * radius * radius
    }

    companion object {
        fun unit() = Circle(1.0)
    }
}

enum class Color {
    RED,
    GREEN,
}

object Registry {
    fun register(name: String) {
        println(name)
    }
}

typealias Names = List<String>

fun distance(x: Double, y: Double): Double {
    return sqrt(x * x + y * y)
}
