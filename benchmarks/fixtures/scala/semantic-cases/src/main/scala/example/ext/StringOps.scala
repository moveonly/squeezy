package example.ext

extension (s: String) def shout: String = s.toUpperCase

extension [T] (xs: List[T]) def secondOpt: Option[T] = xs.drop(1).headOption
