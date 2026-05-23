package util

func Format(name string) string {
	return name
}

type Formatter interface {
	Format(name string) string
}
