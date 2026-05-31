package com.example.services

class FriendlyGreeter : Greeter {
    companion object {
        fun create(): FriendlyGreeter = FriendlyGreeter()
    }

    override fun greet(name: String): String = "Hello, $name"
}
