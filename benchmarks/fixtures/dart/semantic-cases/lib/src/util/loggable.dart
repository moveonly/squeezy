mixin Loggable {
  void log(String msg) {
    print('[${runtimeType}] $msg');
  }
}
