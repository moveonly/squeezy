module Auditable
  def audit!(event)
    log(event)
  end

  def log(event)
    event
  end
end
