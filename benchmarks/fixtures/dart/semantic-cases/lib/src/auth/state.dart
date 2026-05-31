sealed class AuthState {
  const AuthState();
}

class SignedIn extends AuthState {
  final String userId;
  const SignedIn(this.userId);
}

class SignedOut extends AuthState {
  const SignedOut();
}
