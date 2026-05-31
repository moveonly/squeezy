class User < ActiveRecord::Base
  attr_accessor :name, :email

  def full_name
    "#{name} #{surname}"
  end

  def self.find_by_email(email)
    nil
  end

  def surname
    "Surname"
  end
end
