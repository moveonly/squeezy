package greeter

import (
	"fmt"

	util "example.com/squeezy/go-semantic-cases/util"
)

const DefaultName = "Ada"

var Shared = Runner{}

type Greeter interface {
	Greet(name string) string
}

type Runner struct {
	Name string
	Greeter
}

func NewRunner(name string) Runner {
	return Runner{Name: name}
}

func (r Runner) Greet(name string) string {
	fmt.Println(name)
	helper()
	return util.Format(name)
}

func helper() string {
	return DefaultName
}
