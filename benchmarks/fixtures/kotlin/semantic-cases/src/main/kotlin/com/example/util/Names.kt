package com.example.util

import kotlin.text.*

typealias Greeting = String

const val GREETING: Greeting = "Hello"

data class Person(val name: String, val age: Int)

suspend fun fetchDefault(): String = GREETING
