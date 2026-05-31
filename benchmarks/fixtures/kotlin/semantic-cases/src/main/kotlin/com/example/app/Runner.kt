package com.example.app

import com.example.services.FriendlyGreeter as Friendly
import com.example.services.Greeter
import com.example.util.fetchDefault
import com.example.util.prepare

class Runner(private val greeter: Greeter) {
    suspend fun run(): String {
        val name = "world".prepare()
        val default = fetchDefault()
        return greeter.greet(default) + name
    }

    companion object {
        fun buildDefault(): Runner = Runner(Friendly.create())
    }
}
