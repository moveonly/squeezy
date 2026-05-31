package example.services

case class FriendlyGreeter(prefix: String) extends Greeter {
  def greet(name: String): String = s"$prefix, $name"
}
