package com.example.services

sealed interface Greeter {
    fun greet(name: String): String
}

class RudeGreeter : Greeter {
    override fun greet(name: String): String = "go away, $name"
}
