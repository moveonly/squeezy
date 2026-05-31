package example.services

trait Greeter {
  def greet(name: String): String
}

object Greeter {
  def default: Greeter = FriendlyGreeter("Hi")
}
