require_relative "../app/services/greeter"
require_relative "../app/models/user"

def build_runner
  Greeter.new.greet(User.new)
end
