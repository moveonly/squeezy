package example.opaque

opaque type Money = BigDecimal

object Money {
  def apply(x: BigDecimal): Money = x
}

given intOrd: Ordering[Money] = Ordering.by(identity)
