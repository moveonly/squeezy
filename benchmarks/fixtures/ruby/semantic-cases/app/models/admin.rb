require_relative "user"

class Admin < User
  include Auditable

  def promote(user)
    user.full_name
  end
end
