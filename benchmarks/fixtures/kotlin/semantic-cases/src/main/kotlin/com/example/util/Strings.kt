package com.example.util

fun String.prepare(): String = this.trim()

object StringOps {
    fun normalize(s: String): String = s.lowercase()
}
