package example.app

import example.util.*
import example.services.{Greeter, FriendlyGreeter}
import example.ext.*

class Runner(val greeter: Greeter) {
  def run(): String = {
    val name = defaultName.toString
    val greeting = greeter.greet(name)
    "hello".shout
    greeting
  }
}

def buildDefault(): Runner = new Runner(Greeter.default)
